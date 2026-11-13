use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use super::handle::DaemonEvent;
use crate::device_links::connection_log;
use crate::device_links::core::DeviceConnection;
use crate::device_links::device::{
    BatteryStatus, DeviceNotification, DeviceStatus, DeviceView, MediaStatus,
};
use crate::device_links::device_info::DeviceInfo;
use crate::device_links::packet::NetworkPacket;
use crate::device_links::pairing::PairState;
use serde_json::Value;

/// How long a connection error stays on a device that is not reachable: a
/// dial that failed a minute ago says nothing about a phone away for hours.
const ERROR_FRESH_FOR: std::time::Duration = std::time::Duration::from_secs(60);

/// When each device's `last_error` was set ([`mark_error`]). Taken only
/// while holding `devices`, so the two always agree.
static ERROR_AT: std::sync::LazyLock<Mutex<HashMap<String, Instant>>> =
    std::sync::LazyLock::new(Default::default);

/// Whether `error`, set at `marked`, is too old to show on a device that is
/// not reachable. The ladder's "Can't reach … · Try again" is kept: it is the
/// explicit unavailable state, and the way to act on it.
fn error_is_stale(error: &str, marked: Option<Instant>, now: Instant) -> bool {
    !crate::device_links::webrtc::coordinator::is_unavailable(error)
        && marked.is_none_or(|at| now.saturating_duration_since(at) >= ERROR_FRESH_FOR)
}

/// Unpaired devices shown at once; more than any real LAN needs.
const MAX_UNTRUSTED_VIEWS: usize = 32;

/// When each unpaired card was last heard from (beacon or handshake).
static UNTRUSTED_SEEN: std::sync::LazyLock<Mutex<HashMap<String, Instant>>> =
    std::sync::LazyLock::new(Default::default);

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
        if !paired {
            let mut seen = UNTRUSTED_SEEN
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if !devices.contains_key(&info.id)
                && devices.values().filter(|device| !device.trusted).count() >= MAX_UNTRUSTED_VIEWS
            {
                // Full of cards nobody has spoken for: the one heard from longest
                // ago goes (a real phone beacons every few seconds and so stays).
                // Spoofed ids could otherwise fill the cap for good and hide every
                // new phone until restart. A card with a live link or a pair prompt
                // is never the one.
                let oldest = devices
                    .iter()
                    .filter(|(_, device)| {
                        !device.trusted && device.status == DeviceStatus::Discovered
                    })
                    .min_by_key(|(id, _)| seen.get(*id).copied())
                    .map(|(id, _)| id.clone());
                match oldest {
                    Some(id) => {
                        devices.remove(&id);
                        seen.remove(&id);
                    }
                    None => return,
                }
            }
            if seen.len() >= 1024 {
                seen.clear(); // bounded: forgotten cards just read as the oldest
            }
            seen.insert(info.id.clone(), Instant::now());
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

/// Records the version a phone said it runs. Only from an *authenticated*
/// identity (the TLS handshake, pinned for a paired phone): a beacon is
/// anyone's packet, and would make "Update DeskLink" appear on demand.
pub(crate) fn note_app_version(
    devices: &Arc<Mutex<HashMap<String, DeviceView>>>,
    info: &DeviceInfo,
) {
    if let (Some(version), Ok(mut devices)) = (&info.app_version, devices.lock()) {
        if let Some(device) = devices.get_mut(&info.id) {
            device.app_version = Some(version.clone());
        }
    }
}

/// Like [`upsert_device`], for an *unauthenticated* discovery beacon: what an
/// existing paired phone's card says (its name, type, capabilities, address)
/// comes from its authenticated links, never from a packet anybody on the
/// network could have sent; a beacon only creates a card that is not there yet.
pub(crate) fn upsert_from_beacon(
    devices: &Arc<Mutex<HashMap<String, DeviceView>>>,
    info: &DeviceInfo,
    address: String,
    paired: bool,
) {
    let known = devices
        .lock()
        .is_ok_and(|devices| devices.contains_key(&info.id));
    if paired && known {
        return;
    }
    upsert_device(devices, info, address, paired);
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
            app_version: None,
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
    // File writes and other locks wait until the `devices` lock is let go: the
    // UI ticker takes it every second, so a slow disk here froze the window.
    let mut after: Vec<AfterLock> = Vec::new();
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
        after.push(AfterLock::Ready(device_id.clone(), ready));
        // Phone-owned caches (battery, media, notifications) go stale on
        // the edge: kept, shown as "last seen", acted on only once ready.
        if device.feature_transport_ready && !ready {
            device.last_seen = Some(connection_log::now());
            // A call is never kept stale: it cannot be answered without the
            // link, and would ring here forever. Paused players stay paused
            // until the phone's next snapshot (sent on feature-ready).
            if device.call_status.take().is_some() {
                // ...nor kept as "in a call" in the pause/resume bookkeeping:
                // a phone that never comes back would block the resume for
                // every other phone (I13).
                after.push(AfterLock::ReleaseMedia(device_id.clone()));
            }
            // Its ongoing items are as old as the link: gone, and the phone
            // sends them again when it is back (its digest forgets them).
            if device.activity.take().is_some() {
                after.push(AfterLock::ForgetActivity(device_id.clone()));
            }
        } else if ready {
            device.last_seen = None;
        }
        device.feature_transport_ready = ready;
        // A phone on another major.minor that has not become ready is most
        // likely an update away (J15); said in the words the ladder uses.
        if reachable
            && !ready
            && device.last_error.is_none()
            && device.app_version.as_deref().is_some_and(|theirs| {
                crate::device_links::device::versions_skew(crate::config::VERSION, theirs)
            })
        {
            device.last_error = Some(crate::device_links::webrtc::coordinator::update_words(
                &device.name,
            ));
        }

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
            // What went wrong while connecting is news for a minute; after
            // that "Can't reach" says it, and the old words only mislead.
            let marked = ERROR_AT
                .lock()
                .ok()
                .and_then(|at| at.get(device_id).copied());
            if device
                .last_error
                .as_deref()
                .is_some_and(|error| error_is_stale(error, marked, Instant::now()))
            {
                device.last_error = None;
            }
        }
    }
    drop(devices);
    for work in after {
        match work {
            AfterLock::Ready(id, ready) => connection_log::note_ready(&id, ready),
            AfterLock::ReleaseMedia(id) => crate::device_links::calls::hold_media(&id, false),
            AfterLock::ForgetActivity(id) => {
                crate::device_links::sync::forget_feature(&id, "activity")
            }
        }
    }
}

/// What `reconcile_connection_status` does once it no longer holds `devices`.
enum AfterLock {
    Ready(String, bool),
    ReleaseMedia(String),
    ForgetActivity(String),
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
            if let Ok(mut at) = ERROR_AT.lock() {
                at.insert(device_id.to_string(), Instant::now());
            }
        }
    }
}

/// Forgets when `device_id`'s error was set (it was unpaired).
pub(crate) fn forget_error_time(device_id: &str) {
    if let Ok(mut at) = ERROR_AT.lock() {
        at.remove(device_id);
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

pub(crate) fn push_error(errors: &Arc<Mutex<Vec<String>>>, error: String) {
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

/// Tells every main-loop ticker to look now (who is reachable changed): the
/// window refreshes through [`wake_ui`], the others (tray, top bar, players,
/// the Bucket's sampler) through this.
pub(crate) fn nudge_tickers() {
    gtk::glib::MainContext::default().invoke(crate::ticker::nudge_all);
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
    // Decoded here, before the lock the UI also takes.
    let art = packet
        .get_str("albumArtData")
        .map(crate::device_links::media_art::phone_art);
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
            album_art: None,
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
            let url = sanitize_text(album_art_url, 1024);
            // Another track's cover with no picture sent: none shown.
            if art.is_none() && media.album_art_url.as_ref() != Some(&url) {
                media.album_art = None;
            }
            media.album_art_url = Some(url);
        }
        if let Some(art) = art {
            media.album_art = art;
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

/// A phone's Do Not Disturb, from its `desklink.battery` packet (W14): read
/// only while the desk's switch for it is on, else forgotten.
pub(super) fn note_phone_dnd(
    settings: &crate::device_links::config::AppSettings,
    devices: &Arc<Mutex<HashMap<String, DeviceView>>>,
    device_id: &str,
    packet: &NetworkPacket,
) {
    let (dnd, can_set) = if settings.shares_dnd_with(device_id) {
        (
            packet.get_bool("dnd"),
            packet.get_bool("dndCanSet").unwrap_or(false),
        )
    } else {
        (None, false)
    };
    if settings.shares_dnd_with(device_id) {
        crate::device_links::dnd::on_phone(device_id, dnd);
    } else {
        crate::device_links::dnd::forget(device_id);
    }
    if let Ok(mut devices) = devices.lock() {
        if let Some(device) = devices.get_mut(device_id) {
            device.dnd_can_set = dnd.map(|_| can_set);
        }
    }
}

/// `desklink.activity`: the phone's ongoing items now (W13). A phone whose
/// notifications are hidden here keeps none in memory either.
pub(super) fn update_activity(
    settings: &crate::device_links::config::AppSettings,
    devices: &Arc<Mutex<HashMap<String, DeviceView>>>,
    device_id: &str,
    packet: &NetworkPacket,
) -> bool {
    let kept = settings.shows_notifications(device_id);
    let snapshot = kept.then(|| {
        crate::device_links::activity::snapshot_from(
            &packet.body,
            crate::device_links::activity::now_ms(),
        )
    });
    if let Ok(mut devices) = devices.lock() {
        if let Some(device) = devices.get_mut(device_id) {
            device.activity = snapshot;
        }
    }
    // Whether it was kept: one that was dropped is not "held" in the delta
    // digest, or the phone would not send it again when the switch is on.
    kept
}

/// `desklink.telephony {calls, muted, silenced}`: the phone's calls now.
/// Returns whether it is in a call.
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
) -> std::collections::HashSet<String> {
    let ids = string_vec(packet.body.get("ids")).unwrap_or_default();
    let mut missing: std::collections::HashSet<String> = ids.iter().cloned().collect();
    if let Ok(mut devices) = devices.lock() {
        if let Some(device) = devices.get_mut(device_id) {
            device.notification_access = packet.get_bool("accessGranted");
            device
                .notifications
                .retain(|notification| ids.contains(&notification.id));
            for notification in &device.notifications {
                missing.remove(&notification.id);
            }
        }
    }
    missing
}

pub(super) fn upsert_notification(
    devices: &Arc<Mutex<HashMap<String, DeviceView>>>,
    device_id: &str,
    packet: &NetworkPacket,
) -> Option<DeviceNotification> {
    upsert_notification_for(
        &crate::device_links::config::app_settings(),
        devices,
        device_id,
        packet,
    )
}

/// A phone whose notifications are hidden here keeps none in memory either
/// (J12: hiding them hid only the drawing).
pub(super) fn upsert_notification_for(
    settings: &crate::device_links::config::AppSettings,
    devices: &Arc<Mutex<HashMap<String, DeviceView>>>,
    device_id: &str,
    packet: &NetworkPacket,
) -> Option<DeviceNotification> {
    if !settings.shows_notifications(device_id) {
        return None;
    }
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
        actions: action_labels(packet.body.get("actions")),
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
/// A notification's buttons as the desk shows them and sends them back when
/// pressed (`build_notification_action_packet` trims): a blank label (Android
/// 15's redaction blanks every title) or a repeated one could not be pressed,
/// so neither becomes a button.
fn action_labels(value: Option<&Value>) -> Vec<String> {
    let mut labels: Vec<String> = Vec::new();
    for label in string_vec(value).unwrap_or_default() {
        let label = label.trim();
        if !label.is_empty() && !labels.iter().any(|kept| kept == label) {
            labels.push(label.to_string());
        }
    }
    labels
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notification_buttons_drop_blank_and_repeated_labels() {
        let actions = serde_json::json!(["", "  ", " Archive ", "Delete", "Archive", 3]);
        assert_eq!(action_labels(Some(&actions)), ["Archive", "Delete"]);
        // Android 15's redacted notification: three buttons, all blank.
        assert!(action_labels(Some(&serde_json::json!(["", "", ""]))).is_empty());
        assert!(action_labels(None).is_empty());
    }

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

    /// W13: a phone's ongoing items are as old as its link: gone when it
    /// stops being ready (and forgotten by the delta digest, so the phone
    /// sends them again).
    #[test]
    fn ongoing_items_do_not_outlive_the_link() {
        let (devices, info) = test_devices();
        reconcile_connection_status(&devices, &[connection(&info.id, true, true)]);
        let settings = crate::device_links::config::AppSettings::default();
        let mut packet = NetworkPacket::new(crate::device_links::packet::PACKET_TYPE_ACTIVITY);
        packet.set("items", serde_json::json!([{"id": "a", "kind": "timer"}]));
        assert!(update_activity(&settings, &devices, &info.id, &packet));
        reconcile_connection_status(&devices, &[connection(&info.id, true, true)]);
        assert!(devices.lock().unwrap()[&info.id].activity.is_some());
        reconcile_connection_status(&devices, &[]);
        assert!(devices.lock().unwrap()[&info.id].activity.is_none());
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

    /// W13: a snapshot replaces the last, an empty one clears, and a phone
    /// whose notifications are hidden here keeps nothing.
    #[test]
    fn an_activity_snapshot_replaces_the_last_and_hidden_phones_keep_none() {
        let (devices, info) = test_devices();
        let settings = crate::device_links::config::AppSettings::default();
        let mut one = NetworkPacket::new(crate::device_links::packet::PACKET_TYPE_ACTIVITY);
        one.set(
            "items",
            serde_json::json!([{"id": "a", "title": "Pasta", "kind": "timer"}]),
        );
        assert!(update_activity(&settings, &devices, &info.id, &one));
        let held = |devices: &Arc<Mutex<HashMap<String, DeviceView>>>| {
            devices
                .lock()
                .unwrap()
                .get(&info.id)
                .unwrap()
                .activity
                .clone()
        };
        assert_eq!(held(&devices).unwrap().items[0].title, "Pasta");
        let mut none = NetworkPacket::new(crate::device_links::packet::PACKET_TYPE_ACTIVITY);
        none.set("items", serde_json::json!([]));
        assert!(update_activity(&settings, &devices, &info.id, &none));
        assert!(
            held(&devices).unwrap().items.is_empty(),
            "an empty snapshot clears"
        );
        let mut hidden = settings.clone();
        hidden.hidden_notifications.insert(info.id.clone());
        assert!(!update_activity(&hidden, &devices, &info.id, &one));
        assert_eq!(held(&devices), None, "hidden notifications keep none");
    }

    /// W14: the keys are read only while the switch is on; a battery packet
    /// without them leaves what was said.
    #[test]
    fn a_phones_do_not_disturb_is_read_only_while_the_switch_is_on() {
        let (devices, info) = test_devices();
        let mut packet = NetworkPacket::new(crate::device_links::packet::PACKET_TYPE_BATTERY);
        packet.set("currentCharge", -1);
        packet.set("dnd", true);
        packet.set("dndCanSet", true);
        let mut settings = crate::device_links::config::AppSettings::default();
        note_phone_dnd(&settings, &devices, &info.id, &packet);
        let can_set = |devices: &Arc<Mutex<HashMap<String, DeviceView>>>| {
            devices.lock().unwrap().get(&info.id).unwrap().dnd_can_set
        };
        assert_eq!(can_set(&devices), None, "switch off: nothing kept");
        settings.dnd_devices.insert(info.id.clone());
        note_phone_dnd(&settings, &devices, &info.id, &packet);
        assert_eq!(can_set(&devices), Some(true));
        let mut cannot = packet.clone();
        cannot.set("dndCanSet", false);
        note_phone_dnd(&settings, &devices, &info.id, &cannot);
        assert_eq!(can_set(&devices), Some(false));
        crate::device_links::dnd::forget(&info.id);
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

    /// J11: the snapshot's `accessGranted` is kept (absent from an old phone).
    #[test]
    fn a_snapshot_reports_whether_the_phone_may_read_notifications() {
        let (devices, info) = test_devices();
        let access = || devices.lock().unwrap()[&info.id].notification_access;
        let mut snapshot =
            NetworkPacket::new(crate::device_links::packet::PACKET_TYPE_NOTIFICATION);
        snapshot.set("snapshot", true);
        snapshot.set("ids", json!([]));
        replace_notifications(&devices, &info.id, &snapshot);
        assert_eq!(access(), None, "an old phone says nothing");
        snapshot.set("accessGranted", false);
        replace_notifications(&devices, &info.id, &snapshot);
        assert_eq!(access(), Some(false));
        snapshot.set("accessGranted", true);
        replace_notifications(&devices, &info.id, &snapshot);
        assert_eq!(access(), Some(true));
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

    /// Review finding B: a snapshot lists ids whose bodies follow; the ones
    /// not here yet are what the rev must wait for.
    #[test]
    fn a_notification_snapshot_reports_the_bodies_still_to_come() {
        let (devices, info) = test_devices();
        upsert_notification(&devices, &info.id, &notification("n1"));
        let mut snapshot =
            NetworkPacket::new(crate::device_links::packet::PACKET_TYPE_NOTIFICATION);
        snapshot.set("snapshot", true);
        snapshot.set("ids", json!(["n1", "n2", "n3"]));
        let missing = replace_notifications(&devices, &info.id, &snapshot);
        let mut missing: Vec<_> = missing.into_iter().collect();
        missing.sort();
        assert_eq!(missing, ["n2", "n3"]);
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

    /// J15: a phone on another major.minor that is not connected reads as
    /// "Update DeskLink on <phone>"; the same version, or no version, does not.
    #[test]
    fn a_phone_on_another_minor_version_is_asked_to_update() {
        use crate::device_links::device::versions_skew;
        assert!(versions_skew("1.3.0", "1.4.0"));
        assert!(versions_skew("1.3.0", "2.3.0"));
        assert!(!versions_skew("1.3.0", "1.3.7"), "a patch is not skew");
        assert!(!versions_skew("1.3.0", "garbage"));
        let (devices, info) = test_devices();
        let connection = DeviceConnection {
            device_id: info.id.clone(),
            linked: true,
            paired: true,
            feature_transport_ready: false,
        };
        let mut skewed = info.clone();
        skewed.app_version = Some("0.9.0".into());
        // A beacon (upsert) says nothing about the version...
        upsert_device(&devices, &skewed, "10.0.0.2".into(), true);
        reconcile_connection_status(&devices, std::slice::from_ref(&connection));
        assert_eq!(devices.lock().unwrap()[&info.id].last_error, None);
        // ...an authenticated identity does.
        note_app_version(&devices, &skewed);
        // Offline: no message about a phone that is not there.
        let offline = DeviceConnection {
            linked: false,
            ..connection.clone()
        };
        reconcile_connection_status(&devices, std::slice::from_ref(&offline));
        assert_eq!(devices.lock().unwrap()[&info.id].last_error, None);
        reconcile_connection_status(&devices, std::slice::from_ref(&connection));
        let error = devices.lock().unwrap()[&info.id].last_error.clone();
        assert_eq!(
            error.as_deref(),
            Some(crate::device_links::webrtc::coordinator::update_words(&info.name).as_str())
        );
    }

    /// A failed setup is news for a minute; on a phone away for hours it only
    /// misled ("dropped while it was being set up"). The ladder's unavailable
    /// words stay: they carry the Try again.
    #[test]
    fn an_old_connection_error_is_dropped_from_a_phone_that_is_away() {
        let now = Instant::now();
        let eof = "the handshake failed: unexpected EOF";
        assert!(!error_is_stale(eof, Some(now), now));
        assert!(!error_is_stale(
            eof,
            Some(now),
            now + std::time::Duration::from_secs(59)
        ));
        assert!(error_is_stale(eof, Some(now), now + ERROR_FRESH_FOR));
        assert!(error_is_stale(eof, None, now), "set without a time: old");
        let gave_up = crate::device_links::webrtc::coordinator::unavailable_words("Pixel 9");
        assert!(!error_is_stale(&gave_up, None, now + ERROR_FRESH_FOR));

        let (devices, info) = test_devices();
        mark_error(&devices, &info.id, eof.to_string());
        reconcile_connection_status(&devices, &[connection(&info.id, false, false)]);
        assert_eq!(
            devices.lock().unwrap()[&info.id].last_error.as_deref(),
            Some(eof),
            "just failed: still said"
        );
        devices
            .lock()
            .unwrap()
            .get_mut(&info.id)
            .unwrap()
            .last_error = Some("set long ago".into());
        ERROR_AT.lock().unwrap().remove(&info.id);
        reconcile_connection_status(&devices, &[connection(&info.id, false, false)]);
        assert_eq!(devices.lock().unwrap()[&info.id].last_error, None);
    }

    /// I13: a phone that drops out mid-call no longer counts as in a call, so
    /// the other phones' calls can still resume the music.
    #[test]
    fn losing_the_link_ends_the_phones_call_in_the_pause_bookkeeping() {
        let (devices, info) = test_devices();
        let connection = |ready: bool| DeviceConnection {
            device_id: info.id.clone(),
            linked: ready,
            paired: true,
            feature_transport_ready: ready,
        };
        reconcile_connection_status(&devices, &[connection(true)]);
        devices
            .lock()
            .unwrap()
            .get_mut(&info.id)
            .unwrap()
            .call_status = Some(crate::device_links::device::CallStatus {
            calls: Vec::new(),
            muted: false,
            silenced: false,
        });
        crate::device_links::calls::test_hold(&info.id);
        reconcile_connection_status(&devices, &[connection(false)]);
        assert!(!crate::device_links::calls::test_holds(&info.id));
    }

    fn stranger(id: &str) -> DeviceInfo {
        DeviceInfo {
            id: id.to_string(),
            name: "Stranger".to_string(),
            device_type: "phone".to_string(),
            protocol_version: 9,
            incoming_capabilities: Vec::new(),
            outgoing_capabilities: Vec::new(),
            instance_id: None,
            app_version: None,
        }
    }

    #[test]
    fn spoofed_cards_cannot_fill_the_list_for_good() {
        let devices = Arc::new(Mutex::new(HashMap::new()));
        let tag = uuid::Uuid::new_v4().simple().to_string();
        let id = |n: usize| format!("{tag}-{n:03}");
        // A real phone's card is heard from early; then a flood of invented ids.
        upsert_device(&devices, &stranger(&id(0)), "10.0.0.2".into(), false);
        for n in 1..=MAX_UNTRUSTED_VIEWS {
            upsert_device(&devices, &stranger(&id(n)), "10.0.0.3".into(), false);
        }
        {
            let devices = devices.lock().unwrap();
            assert_eq!(devices.len(), MAX_UNTRUSTED_VIEWS, "the cap holds");
            assert!(
                !devices.contains_key(&id(0)),
                "the card heard from longest ago made room"
            );
            assert!(
                devices.contains_key(&id(MAX_UNTRUSTED_VIEWS)),
                "the newest is listed"
            );
        }
        // A real phone that beacons again is the newest, however many came before.
        upsert_device(&devices, &stranger(&id(1)), "10.0.0.3".into(), false);
        upsert_device(&devices, &stranger(&id(99)), "10.0.0.4".into(), false);
        let devices = devices.lock().unwrap();
        assert!(
            devices.contains_key(&id(1)),
            "a card that keeps beaconing stays"
        );
        assert!(!devices.contains_key(&id(2)));
    }

    #[test]
    fn a_card_with_a_live_link_is_never_the_one_that_makes_room() {
        let devices = Arc::new(Mutex::new(HashMap::new()));
        let tag = uuid::Uuid::new_v4().simple().to_string();
        for n in 0..MAX_UNTRUSTED_VIEWS {
            upsert_device(
                &devices,
                &stranger(&format!("{tag}-{n:03}")),
                "10.0.0.3".into(),
                false,
            );
        }
        for device in devices.lock().unwrap().values_mut() {
            device.status = DeviceStatus::Connected;
        }
        upsert_device(
            &devices,
            &stranger(&format!("{tag}-new")),
            "10.0.0.4".into(),
            false,
        );
        let devices = devices.lock().unwrap();
        assert_eq!(devices.len(), MAX_UNTRUSTED_VIEWS);
        assert!(
            !devices.contains_key(&format!("{tag}-new")),
            "nothing evictable: refused"
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
    fn a_hidden_phones_notifications_are_not_kept() {
        let (devices, info) = test_devices();
        let mut packet = NetworkPacket::new(crate::device_links::packet::PACKET_TYPE_NOTIFICATION);
        packet.set("id", "n1");
        packet.set("appName", "Chat");
        let mut settings = crate::device_links::config::AppSettings::default();
        settings.hidden_notifications.insert(info.id.clone());
        assert!(upsert_notification_for(&settings, &devices, &info.id, &packet).is_none());
        assert!(devices.lock().unwrap()[&info.id].notifications.is_empty());
        settings.hidden_notifications.clear();
        assert!(upsert_notification_for(&settings, &devices, &info.id, &packet).is_some());
    }

    #[test]
    fn a_notification_keeps_no_blank_action_titles() {
        let (devices, info) = test_devices();
        let mut packet = notification("sensitive");
        packet.set("actions", serde_json::json!(["", " ", "Archive"]));
        let settings = crate::device_links::config::AppSettings::default();
        let kept = upsert_notification_for(&settings, &devices, &info.id, &packet).expect("kept");
        assert_eq!(kept.actions, vec!["Archive".to_string()]);
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
    fn the_phones_album_art_data_is_shown_and_a_bad_one_ignored() {
        use base64::Engine as _;
        use gtk::gdk_pixbuf::{Colorspace, Pixbuf};
        let picture = Pixbuf::new(Colorspace::Rgb, false, 8, 400, 300).unwrap();
        picture.fill(0x3366_99ff);
        let jpeg = picture
            .save_to_bufferv("jpeg", &[("quality", "80")])
            .unwrap();
        let data = base64::engine::general_purpose::STANDARD.encode(&jpeg);
        let (devices, info) = test_devices();
        let mpris = |art: &str, url: &str| {
            let mut packet = NetworkPacket::new(crate::device_links::packet::PACKET_TYPE_MPRIS);
            packet.set("player", "Spotify");
            packet.set("title", "Track");
            packet.set("albumArtUrl", url);
            packet.set("albumArtData", art);
            packet
        };
        let cover = |devices: &Arc<Mutex<HashMap<String, DeviceView>>>| {
            let mut devices = devices.lock().unwrap();
            let device = devices.get_mut(&info.id).unwrap();
            device.trusted = true;
            device.feature_transport_ready = true;
            let art = device.media_status.as_ref().unwrap().album_art.clone();
            (art, crate::phone_mpris::shown_for(device).unwrap().art_url)
        };

        update_media_from_packet(&devices, &info.id, &mpris(&data, "content://a"));
        let (art, url) = cover(&devices);
        let art = art.expect("the cover is shown");
        assert_eq!((art.width, art.height), (320, 240), "scaled to MAX_SIDE");
        assert_eq!(art.pixels.len(), art.stride * 240);
        let file = art.file.clone().unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), jpeg);
        assert!(
            url.unwrap().starts_with("file://"),
            "the MPRIS player points at it"
        );

        let oversized = "A".repeat(crate::device_links::media_art::MAX_JPEG_BYTES / 3 * 4 + 8);
        let png = base64::engine::general_purpose::STANDARD
            .encode(picture.save_to_bufferv("png", &[]).unwrap());
        let mut not_jpeg = jpeg.clone();
        not_jpeg.truncate(40);
        let truncated = base64::engine::general_purpose::STANDARD.encode(&not_jpeg);
        for bad in [
            oversized.as_str(),
            "%%not base64%%",
            png.as_str(),
            truncated.as_str(),
            "",
        ] {
            update_media_from_packet(&devices, &info.id, &mpris(&data, "content://a"));
            update_media_from_packet(&devices, &info.id, &mpris(bad, "content://b"));
            let (art, url) = cover(&devices);
            assert!(art.is_none(), "ignored: {}", &bad[..bad.len().min(16)]);
            assert_eq!(url, None);
        }
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
}
