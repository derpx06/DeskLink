//! The desk's side of the features a phone drives that are not a single
//! packet handler: the desk's settings (reported and changed from the phone),
//! unlock with the phone, and Lock & Hand Off.
//!
//! The daemon installs its session manager, config and device table here
//! once, so pushes can start from any thread (a settings change at the desk,
//! a player starting). What each packet says is decided by the `*_packet` and
//! `parse_*` functions, which have no I/O and are unit-tested.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::config::{AppSettings, Config};
use super::connection_log::{record_detail, ConnectionEvent};
use super::core::DeviceManager;
use super::device::{DeviceStatus, DeviceView};
use super::packet::{
    NetworkPacket, PACKET_TYPE_HANDOFF, PACKET_TYPE_LOCK, PACKET_TYPE_SETTINGS,
    PACKET_TYPE_UNLOCK_REQUEST,
};
use super::transport::FeatureTransport;
use super::unlock::{self, Conditions, Refusal};

struct Daemon {
    sessions: Arc<DeviceManager>,
    config: Arc<Mutex<Config>>,
    devices: Arc<Mutex<HashMap<String, DeviceView>>>,
    /// Where a visible error goes (the window's toast).
    errors: Arc<Mutex<Vec<String>>>,
}

static DAEMON: OnceLock<Daemon> = OnceLock::new();

/// Called once by the daemon at startup.
pub(crate) fn install(
    sessions: Arc<DeviceManager>,
    config: Arc<Mutex<Config>>,
    devices: Arc<Mutex<HashMap<String, DeviceView>>>,
    errors: Arc<Mutex<Vec<String>>>,
) {
    let _ = DAEMON.set(Daemon {
        sessions,
        config,
        devices,
        errors,
    });
}

/// How long `device_id` has been silent on every channel: the shorter of the
/// LAN link's silence and the data channel's.
pub(crate) fn inbound_silence(device_id: &str) -> Option<std::time::Duration> {
    let lan = DAEMON.get()?.sessions.silence(device_id);
    super::webrtc::coordinator::link_silence(
        lan,
        super::webrtc::coordinator::data_silence(device_id),
    )
}

/// The phones with their encrypted connection up.
pub(crate) fn ready_devices() -> Vec<String> {
    let Some(daemon) = DAEMON.get() else {
        return Vec::new();
    };
    daemon
        .sessions
        .current_bindings()
        .into_iter()
        .filter(|binding| {
            FeatureTransport::for_binding(&daemon.sessions, binding)
                .is_ok_and(|transport| transport.is_ready())
        })
        .map(|binding| binding.device_id)
        .collect()
}

/// Sends `packet` to `device_id` if its connection is up; quietly drops it
/// otherwise (a phone that reconnects gets the current state then).
pub(crate) fn send(device_id: &str, packet: &NetworkPacket) {
    #[cfg(test)]
    if device_id.starts_with(TEST_OUTBOX_PREFIX) {
        if let Ok(mut sent) = TEST_SENT.lock() {
            sent.push((device_id.to_string(), packet.clone()));
        }
        return;
    }
    let Some(daemon) = DAEMON.get() else {
        return;
    };
    let result = FeatureTransport::for_device(&daemon.sessions, device_id).and_then(|transport| {
        if transport.is_ready() {
            transport.send(packet)
        } else {
            Ok(())
        }
    });
    if let Err(error) = result {
        debug_log!(
            "[DeskLink] {} not sent to {device_id}: {error}",
            packet.packet_type
        );
    }
}

/// Tests: packets sent to a device whose id starts with this are kept here
/// instead of going to a connection.
#[cfg(test)]
pub(crate) const TEST_OUTBOX_PREFIX: &str = "test-outbox-";

#[cfg(test)]
static TEST_SENT: Mutex<Vec<(String, NetworkPacket)>> = Mutex::new(Vec::new());

/// Tests: what was sent to `device_id`, oldest first.
#[cfg(test)]
pub(crate) fn test_sent(device_id: &str) -> Vec<NetworkPacket> {
    TEST_SENT
        .lock()
        .map(|sent| {
            sent.iter()
                .filter(|(device, _)| device == device_id)
                .map(|(_, packet)| packet.clone())
                .collect()
        })
        .unwrap_or_default()
}

/// Work for the one desk-features thread, waiting.
type Job = Box<dyn FnOnce() + Send>;

/// How many jobs may wait; beyond that a peer is flooding and they are
/// dropped.
const QUEUE: usize = 16;

/// Runs `job` on the desk-features thread (D-Bus, logind and config writes
/// stay off the peer worker, and a flood costs one thread, not one per
/// packet). False when it was dropped because the queue was full.
pub(crate) fn run_later(job: impl FnOnce() + Send + 'static) -> bool {
    let queued = try_queue(Box::new(job)).is_ok();
    if !queued {
        report_dropped();
    }
    queued
}

/// Queues `job` for the desk thread, or hands it back when the queue is full.
/// Every job runs under [`crate::guard`]: one that panics must not end the
/// one thread all the others depend on.
fn try_queue(job: Job) -> Result<(), Job> {
    static WORKER: OnceLock<std::sync::mpsc::SyncSender<Job>> = OnceLock::new();
    let sender = WORKER.get_or_init(|| {
        let (sender, jobs) = std::sync::mpsc::sync_channel::<Job>(QUEUE);
        let _ = std::thread::Builder::new()
            .name("desklink-desk".into())
            .spawn(move || {
                for job in jobs {
                    crate::guard("desk job", (), job);
                }
            });
        sender
    });
    sender.try_send(job).map_err(|error| match error {
        std::sync::mpsc::TrySendError::Full(job)
        | std::sync::mpsc::TrySendError::Disconnected(job) => job,
    })
}

/// How long a job that must not be lost waits for room in the queue.
const MUST_RUN_WITHIN: Duration = Duration::from_secs(10);

/// Like [`run_later`] for work the sync cannot do without (the `have`, the
/// snapshots): a full queue is waited out, not a dropped job. The wait is on a
/// thread of its own, so the caller (a peer worker, a timer) never blocks.
pub(crate) fn run_later_reliably(job: impl FnOnce() + Send + 'static) {
    let Err(mut waiting) = try_queue(Box::new(job)) else {
        return;
    };
    // A flood still costs a bounded number of waiting threads.
    static WAITING: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    const MAX_WAITING: usize = 8;
    if WAITING.fetch_add(1, std::sync::atomic::Ordering::AcqRel) >= MAX_WAITING {
        WAITING.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
        report_dropped();
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("desklink-desk-retry".into())
        .spawn(move || {
            let until = Instant::now() + MUST_RUN_WITHIN;
            let mut queued = false;
            while Instant::now() < until {
                std::thread::sleep(Duration::from_millis(50));
                match try_queue(waiting) {
                    Ok(()) => {
                        queued = true;
                        break;
                    }
                    Err(job) => waiting = job,
                }
            }
            WAITING.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
            if !queued {
                report_dropped();
            }
        });
    if spawned.is_err() {
        WAITING.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
        report_dropped();
    }
}

/// Jobs dropped because the queue was full, since start.
static DROPPED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// A dropped job is told to the user at most this often.
const DROP_NOTICE_EVERY: Duration = Duration::from_secs(30);

#[cfg(test)]
pub(crate) fn dropped_jobs() -> u64 {
    DROPPED.load(std::sync::atomic::Ordering::Relaxed)
}

/// What the user is told when requests were dropped.
pub(crate) fn drop_notice(count: u64) -> String {
    match count {
        1 => "DeskLink was busy and ignored a request from a phone. Try again.".to_string(),
        count => format!("DeskLink was busy and ignored {count} requests from phones. Try again."),
    }
}

/// A full queue is visible, not silent: counted, logged, and shown once in a
/// while (a flood must not become a flood of toasts).
fn report_dropped() {
    static LAST: Mutex<Option<(Instant, u64)>> = Mutex::new(None);
    let total = DROPPED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    debug_log!("[DeskLink] Desk request dropped: too many waiting ({total} so far)");
    let Ok(mut last) = LAST.lock() else {
        return;
    };
    let due = last.is_none_or(|(at, _)| at.elapsed() >= DROP_NOTICE_EVERY);
    if !due {
        return;
    }
    let since = last.map_or(0, |(_, counted)| counted);
    *last = Some((Instant::now(), total));
    if let Some(daemon) = DAEMON.get() {
        super::daemon::push_user_error(&daemon.errors, drop_notice(total - since));
    }
}
fn device(device_id: &str) -> Option<DeviceView> {
    DAEMON.get()?.devices.lock().ok()?.get(device_id).cloned()
}

/// The phone's process (its identity's `instanceId`), if known.
fn peer_instance(device_id: &str) -> Option<String> {
    DAEMON
        .get()?
        .sessions
        .current_bindings()
        .into_iter()
        .find(|binding| binding.device_id == device_id)
        .and_then(|binding| binding.link.info.instance_id.clone())
}

/// This process's `instanceId`.
fn own_instance() -> String {
    DAEMON
        .get()
        .and_then(|daemon| daemon.config.lock().ok())
        .and_then(|config| config.local_device_info().instance_id)
        .unwrap_or_default()
}

/// A phone's encrypted connection just came up: first what this side already
/// holds from it (`desklink.sync.have`), then — after the phone's own, or
/// [`super::sync::SYNC_WAIT`] — the desk's state: only what differs.
pub(crate) fn on_feature_ready(device_id: &str) {
    let device_id = device_id.to_string();
    // The connection this ready belongs to: a job that runs after it ended
    // (or after another began) must not speak for it.
    let connection = current_connection(&device_id);
    run_later_reliably(move || {
        if !same_connection(&connection, &current_connection(&device_id)) {
            return;
        }
        send(
            &device_id,
            &super::sync::have_packet(&device_id, peer_instance(&device_id).as_deref()),
        );
        // Folder sync (F8): runs only while connected. Without this the
        // Bucket's worker never counts the phone as ready and nothing syncs.
        super::folders::link(&device_id, true);
        match super::sync::begin_wait(&device_id) {
            // Their `have` came before our ready: send now.
            None => send_initial_snapshots(&device_id),
            Some(token) => {
                let spawned = std::thread::Builder::new()
                    .name("desklink-sync-wait".into())
                    .spawn({
                        let (device_id, connection) = (device_id.clone(), connection.clone());
                        move || {
                            std::thread::sleep(super::sync::SYNC_WAIT);
                            if super::sync::release_wait(&device_id, token) {
                                run_later_reliably(move || {
                                    if same_connection(&connection, &current_connection(&device_id))
                                    {
                                        send_initial_snapshots(&device_id)
                                    }
                                });
                            }
                        }
                    });
                if spawned.is_err() {
                    // No timer thread: the snapshots go now rather than never.
                    if super::sync::release_wait(&device_id, token) {
                        send_initial_snapshots(&device_id);
                    }
                }
            }
        }
    });
}

/// Which connection a job belongs to: the WebRTC attempt of the phone's
/// session (a rebuilt peer is a new connection).
pub(crate) type Connection = Option<String>;

/// The phone's current connection, as far as this process knows.
pub(crate) fn current_connection(device_id: &str) -> Connection {
    let sessions = &DAEMON.get()?.sessions;
    let binding = sessions
        .current_bindings()
        .into_iter()
        .find(|binding| binding.device_id == device_id)?;
    sessions.active_webrtc_attempt(&binding).ok().flatten()
}

fn same_connection(expected: &Connection, now: &Connection) -> bool {
    expected == now
}

/// A phone's `desklink.sync.have`, received on `connection`: kept for it, and
/// if the desk's snapshots were waiting for it, sent now. One that arrives
/// after its connection ended says nothing about the new one.
pub(crate) fn on_sync_have(device_id: &str, packet: &NetworkPacket, connection: &Connection) {
    if apply_have(
        device_id,
        packet,
        connection,
        &current_connection(device_id),
    ) {
        let (device_id, connection) = (device_id.to_string(), connection.clone());
        run_later_reliably(move || {
            if same_connection(&connection, &current_connection(&device_id)) {
                send_initial_snapshots(&device_id)
            }
        });
    }
}

/// Whether `packet` was kept: only on the connection it arrived on. Whether a
/// send was waiting for it.
fn apply_have(
    device_id: &str,
    packet: &NetworkPacket,
    arrived_on: &Connection,
    current: &Connection,
) -> bool {
    same_connection(arrived_on, current) && super::sync::on_have(device_id, packet)
}

/// The media state and its artwork.
type MediaSnapshot = (NetworkPacket, Option<Arc<super::media_art::Art>>);

/// The desk's snapshots for a phone that just connected, each only when the
/// phone's `have` (`theirs`) does not already hold this very version
/// (everything when it saw an older process of ours — `own` differs — or said
/// nothing): the settings, the media, the notifications, the snippets and
/// the notes.
fn initial_snapshots(
    device_id: &str,
    theirs: Option<&super::sync::Theirs>,
    own: &str,
    accepts_notifications: bool,
) -> (Vec<NetworkPacket>, Option<MediaSnapshot>) {
    let wanted = |feature: &str, rev: &str| super::sync::should_send(theirs, own, feature, rev);
    let mut packets = Vec::new();
    let settings = settings_packet(&super::config::app_settings(), device_id);
    if wanted("settings", settings.get_str("rev").unwrap_or_default()) {
        packets.push(settings);
    }
    let media = super::mpris::state_snapshot()
        .filter(|snapshot| wanted("media", snapshot.0.get_str("rev").unwrap_or_default()));
    // This computer's notifications (C1): the snapshot, then bodies.
    let notifications = super::desk_notifications::snapshot_packets();
    let rev = notifications
        .first()
        .and_then(|packet| packet.get_str("rev"))
        .unwrap_or_default()
        .to_string();
    if accepts_notifications && wanted("notifications", &rev) {
        packets.extend(notifications);
    }
    // Snippets and notes (F6): what is here; the phone fetches.
    for mut group in super::notes::indexes() {
        let Some(first) = group.first() else {
            continue;
        };
        let feature = if first.packet_type == super::packet::PACKET_TYPE_SNIPPETS {
            "snippets"
        } else {
            "notes"
        };
        // One rev for every part of the index.
        let rev = super::sync::rev_of(&group, &[]);
        for part in &mut group {
            part.set("rev", rev.as_str());
        }
        if wanted(feature, &rev) {
            packets.extend(group);
        }
    }
    (packets, media)
}

fn send_initial_snapshots(device_id: &str) {
    let theirs = super::sync::theirs(device_id);
    let accepts = super::features::peer_accepts(device_id, super::packet::PACKET_TYPE_NOTIFICATION);
    let (packets, media) = initial_snapshots(device_id, theirs.as_ref(), &own_instance(), accepts);
    for packet in &packets {
        send(device_id, packet);
    }
    if let Some(snapshot) = media {
        super::mpris::forget_art_sent(device_id);
        super::mpris::send_snapshot_to(device_id, snapshot);
    }
}

// ── Settings ───────────────────────────────────────────────────────────────

/// `desklink.settings` for `device_id`: every key, always.
pub(crate) fn settings_packet(settings: &AppSettings, device_id: &str) -> NetworkPacket {
    let mut packet = NetworkPacket::new(PACKET_TYPE_SETTINGS);
    packet.set("keepAwake", settings.stay_awake);
    packet.set("clipboard", settings.clipboard_sync);
    packet.set("notifications", settings.shows_notifications(device_id));
    packet.set("relayConfigured", settings.relay_configured());
    packet.set("relay", settings.relay_wanted());
    packet.set("commandAlerts", settings.alerts_commands_to(device_id));
    packet.set("unlockNearby", settings.unlocks_from(device_id));
    packet.set("sessionPasswordSet", settings.session_password.is_some());
    let rev = super::sync::rev_of(std::slice::from_ref(&packet), &[]);
    packet.set("rev", rev);
    packet
}

/// Sends every connected phone the desk's settings. Called after any change.
pub(crate) fn push_settings() {
    let settings = super::config::app_settings();
    for device_id in ready_devices() {
        send(&device_id, &settings_packet(&settings, &device_id));
    }
}

/// The allow-listed changes in a `desklink.settings.request`: `(key, value)`.
/// Unknown keys, non-booleans, `unlockNearby` (desk only) and `relay` while
/// no relay is set up are dropped.
pub(crate) fn parse_settings_request(
    packet: &NetworkPacket,
    relay_configured: bool,
) -> Vec<(String, bool)> {
    let Some(Value::Object(set)) = packet.body.get("set") else {
        return Vec::new();
    };
    let mut changes: Vec<(String, bool)> = set
        .iter()
        .filter_map(|(key, value)| Some((key.clone(), value.as_bool()?)))
        .filter(|(key, _)| match key.as_str() {
            // Not `keepAwake`: it holds the laptop awake with the lid closed,
            // so only the person at this desk turns it on (a phone that asks
            // was a flat battery and a hot bag).
            "clipboard" | "notifications" | "commandAlerts" => true,
            "relay" => relay_configured,
            _ => false,
        })
        .collect();
    changes.sort();
    changes
}

/// `key`'s value now, for `device_id`.
fn current_value(settings: &AppSettings, device_id: &str, key: &str) -> Option<bool> {
    Some(match key {
        "keepAwake" => settings.stay_awake,
        "clipboard" => settings.clipboard_sync,
        "notifications" => settings.shows_notifications(device_id),
        "relay" => settings.relay_enabled,
        "commandAlerts" => settings.alerts_commands_to(device_id),
        _ => return None,
    })
}

/// The changes that change something: a phone resending its view of the
/// settings must not save, log or reset the relay.
fn real_changes(
    settings: &AppSettings,
    device_id: &str,
    changes: Vec<(String, bool)>,
) -> Vec<(String, bool)> {
    changes
        .into_iter()
        .filter(|(key, value)| current_value(settings, device_id, key) != Some(*value))
        .collect()
}

/// Applies `change` to `settings` for `device_id`.
fn apply_change(settings: &mut AppSettings, device_id: &str, key: &str, value: bool) {
    match key {
        "clipboard" => settings.clipboard_sync = value,
        "notifications" => {
            if value {
                settings.hidden_notifications.remove(device_id);
            } else {
                settings.hidden_notifications.insert(device_id.to_string());
            }
        }
        "relay" => settings.relay_enabled = value,
        "commandAlerts" => {
            if value {
                settings.muted_command_alerts.remove(device_id);
            } else {
                settings.muted_command_alerts.insert(device_id.to_string());
            }
        }
        _ => {}
    }
}

/// A phone changed some of the desk's settings: apply, log, and tell every
/// phone (the asker included) the result.
pub(crate) fn handle_settings_request(device_id: &str, packet: &NetworkPacket) {
    let Some(daemon) = DAEMON.get() else {
        return;
    };
    let settings = super::config::app_settings();
    let changes = real_changes(
        &settings,
        device_id,
        parse_settings_request(packet, settings.relay_configured()),
    );
    if !changes.is_empty() {
        let saved = daemon
            .config
            .lock()
            .map_err(|_| "Config lock poisoned".to_string())
            .and_then(|mut config| {
                config.update_settings(|settings| {
                    for (key, value) in &changes {
                        apply_change(settings, device_id, key, *value);
                    }
                })
            });
        match saved {
            Ok(()) => {
                for (key, value) in &changes {
                    record_detail(
                        device_id,
                        ConnectionEvent::SettingsChanged,
                        &format!("{key}={}", if *value { "on" } else { "off" }),
                    );
                }
                if changes.iter().any(|(key, _)| key == "relay") {
                    super::relay_link::client::reset();
                }
            }
            Err(error) => eprintln!("[DeskLink] Could not save a setting from the phone: {error}"),
        }
    }
    push_settings();
}

/// Sends a synced file to `device_id` (F8), if its connection is ready.
pub(crate) fn send_folder_file(
    device_id: &str,
    path: &std::path::Path,
    folder: crate::device_links::webrtc::file_protocol::FolderRef,
    content_sha256: &str,
) -> Result<(), String> {
    let daemon = DAEMON.get().ok_or("DeskLink is starting")?;
    let transport = FeatureTransport::for_device(&daemon.sessions, device_id)?;
    if !transport.is_ready() {
        return Err("The phone is not connected".into());
    }
    transport.send_folder_file(path, folder, content_sha256)
}

/// Cancels a transfer (F8c: an item removed while it moves). The phone is
/// told it was `removed`, so both sides clean up.
pub(crate) fn cancel_transfer(device_id: &str, transfer_id: &str) -> Result<(), String> {
    let daemon = DAEMON.get().ok_or("DeskLink is starting")?;
    FeatureTransport::for_device(&daemon.sessions, device_id)?.cancel_transfer(
        transfer_id,
        crate::device_links::webrtc::file_protocol::code::REMOVED,
    )
}

/// Remembers a folder a phone started a terminal in (newest first, five).
pub(crate) fn remember_folder(folder: &str) {
    let Some(daemon) = DAEMON.get() else {
        return;
    };
    let folder = folder.to_string();
    if let Ok(mut config) = daemon.config.lock() {
        let _ = config.update_settings(|settings| {
            settings.recent_folders.retain(|known| *known != folder);
            settings.recent_folders.insert(0, folder);
            settings.recent_folders.truncate(5);
        });
    }
}

// ── Unlock with the phone ──────────────────────────────────────────────────

/// What the desk knows when deciding about an unlock for `device_id`.
fn conditions(device_id: &str) -> Conditions {
    Conditions {
        allowed: super::config::app_settings().unlocks_from(device_id),
        locked: super::daemon::desktop_locked(),
        ..device_conditions(device(device_id).as_ref())
    }
}

/// What the device's connection says: on this network ("lan": a host or
/// peer-reflexive pair, not bootstrapped through the relay), and ready.
fn device_conditions(device: Option<&DeviceView>) -> Conditions {
    Conditions {
        allowed: false,
        locked: false,
        // "lan" is only the route type (host/prflx pair): a global IPv6 path or a
        // VPN-routed subnet is one too. An unlock needs the phone to be near.
        local: device.is_some_and(|device| {
            device.connection_path.as_deref() == Some("lan") && device.connection_nearby
        }),
        ready: device.is_some_and(|device| {
            device.status == DeviceStatus::Paired && device.feature_transport_ready
        }),
    }
}

/// Offers `device_id` an unlock (`reason` "button" or "phone") if allowed,
/// else refuses. Returns whether a nonce went out.
pub(crate) fn offer_unlock(device_id: &str, reason: &str) -> bool {
    match unlock::may_offer(conditions(device_id)) {
        Ok(()) => {
            let mut packet = NetworkPacket::new(PACKET_TYPE_UNLOCK_REQUEST);
            packet.set("nonce", unlock::issue(device_id));
            packet.set("reason", reason);
            send(device_id, &packet);
            true
        }
        Err(refusal) => {
            refuse(device_id, refusal);
            false
        }
    }
}

/// `desklink.unlock.request` from the phone: `{ask: true}` asks for a nonce.
pub(crate) fn handle_unlock_request(device_id: &str, packet: &NetworkPacket) {
    if packet.get_bool("ask").unwrap_or(false) {
        offer_unlock(device_id, "phone");
    }
}

/// The phone approved an unlock (`setLocked: false` with the nonce).
pub(crate) fn handle_unlock_approval(device_id: &str, nonce: Option<&str>) {
    // The same flood damper as locking: one operation per settle window.
    if !super::daemon::admit_lock_operation() {
        return;
    }
    match unlock::approve(device_id, nonce, conditions(device_id)) {
        Ok(()) => match super::logind::unlock() {
            Ok(()) => record_detail(device_id, ConnectionEvent::UnlockedByPhone, "nearby"),
            Err(error) => {
                eprintln!("[DeskLink] Could not unlock the session: {error}");
                refuse(device_id, Refusal::NotLocked);
            }
        },
        Err(refusal) => refuse(device_id, refusal),
    }
}

/// The same refusal again this soon is not logged again.
const REFUSAL_LOG_WINDOW: Duration = Duration::from_secs(30);

/// Whether a refusal is news for the history: not the same device and code
/// as the last one logged within [`REFUSAL_LOG_WINDOW`].
fn refusal_is_news(
    last: &mut Option<(String, Refusal, Instant)>,
    device_id: &str,
    refusal: Refusal,
    now: Instant,
) -> bool {
    if let Some((device, code, at)) = last {
        if device == device_id && *code == refusal && now.duration_since(*at) < REFUSAL_LOG_WINDOW {
            return false;
        }
    }
    *last = Some((device_id.to_string(), refusal, now));
    true
}

/// `desklink.lock {isLocked, unlockRefused}`, and a history entry.
fn refuse(device_id: &str, refusal: Refusal) {
    static LAST: Mutex<Option<(String, Refusal, Instant)>> = Mutex::new(None);
    if LAST
        .lock()
        .is_ok_and(|mut last| refusal_is_news(&mut last, device_id, refusal, Instant::now()))
    {
        record_detail(device_id, ConnectionEvent::UnlockRefused, refusal.code());
    }
    let mut packet = NetworkPacket::new(PACKET_TYPE_LOCK);
    packet.set("isLocked", super::daemon::desktop_locked());
    packet.set("unlockRefused", refusal.code());
    send(device_id, &packet);
}

/// Whether `device` could unlock this computer were it locked: the setting
/// on for this phone, and the phone connected on this network. The
/// lock-screen button.
pub(crate) fn could_unlock(device: &DeviceView, settings: &AppSettings) -> bool {
    let conditions = Conditions {
        allowed: settings.unlocks_from(&device.id),
        locked: true,
        ..device_conditions(Some(device))
    };
    unlock::may_offer(conditions).is_ok()
}

// ── Lock & Hand Off ────────────────────────────────────────────────────────

/// `desklink.handoff` for one terminal.
pub(crate) fn handoff_packet(session_id: &str, title: &str, path: &str) -> NetworkPacket {
    let mut packet = NetworkPacket::new(PACKET_TYPE_HANDOFF);
    packet.set("sessionId", session_id);
    packet.set("title", title);
    packet.set("path", path);
    packet
}

/// Locks this computer, always and first, then offers the newest terminal
/// to every connected phone with terminal access (best effort).
pub(crate) fn lock_and_hand_off() -> Result<(), String> {
    let phones: Vec<(String, bool)> = ready_devices()
        .into_iter()
        .map(|id| {
            let access = super::config::terminal_access_allowed(&id);
            (id, access)
        })
        .collect();
    hand_off(
        super::daemon::lock_desktop,
        super::pty::registry::newest_session().as_ref(),
        &phones,
        send,
    )
}

/// [`lock_and_hand_off`] with its effects passed in: `lock` runs first,
/// whatever follows; `phones` is each connected phone and its terminal
/// access.
fn hand_off(
    lock: impl FnOnce(),
    newest: Option<&super::pty::registry::SessionInfo>,
    phones: &[(String, bool)],
    mut send: impl FnMut(&str, &NetworkPacket),
) -> Result<(), String> {
    lock();
    let newest = newest.ok_or("Locked. No terminal was open to hand off")?;
    let packet = handoff_packet(
        &newest.id,
        &newest.title,
        newest.cwd.as_deref().unwrap_or("~"),
    );
    let mut sent = 0;
    for (phone, _) in phones.iter().filter(|(_, access)| *access) {
        send(phone, &packet);
        sent += 1;
    }
    if sent == 0 {
        return Err("Locked. No phone with terminal access is connected".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Delta resync, desk side: on a reconnect of the same processes only what
    /// differs goes; a restart of either side sends everything.
    #[test]
    fn a_reconnect_sends_only_what_changed_and_a_new_epoch_sends_everything() {
        let device = "delta-phone";
        let own = "desk-process-1";
        let (all, _) = initial_snapshots(device, None, own, true);
        let types = |packets: &[NetworkPacket]| -> Vec<String> {
            packets.iter().map(|p| p.packet_type.clone()).collect()
        };
        assert!(
            types(&all).contains(&PACKET_TYPE_SETTINGS.to_string()),
            "no `have`: everything"
        );
        assert!(
            all.iter().all(|p| p.get_str("rev").is_some()),
            "every snapshot carries its rev"
        );
        let notifications_rev = all
            .iter()
            .find(|p| p.packet_type == super::super::packet::PACKET_TYPE_NOTIFICATION)
            .and_then(|p| p.get_str("rev"))
            .map(str::to_string)
            .expect("the notifications snapshot");
        let settings_rev = all
            .iter()
            .find(|p| p.packet_type == PACKET_TYPE_SETTINGS)
            .and_then(|p| p.get_str("rev"))
            .map(str::to_string)
            .unwrap();
        // The phone holds this very settings and notifications of this process.
        let theirs = crate::device_links::sync::Theirs {
            peer_instance: own.to_string(),
            have: [
                ("settings".to_string(), settings_rev),
                ("notifications".to_string(), notifications_rev),
            ]
            .into(),
        };
        let (delta, _) = initial_snapshots(device, Some(&theirs), own, true);
        assert!(!types(&delta).contains(&PACKET_TYPE_SETTINGS.to_string()));
        assert!(
            !types(&delta).contains(&super::super::packet::PACKET_TYPE_NOTIFICATION.to_string())
        );
        // Settings changed meanwhile: only those go.
        let mut changed = theirs.clone();
        changed
            .have
            .insert("settings".into(), "an-older-rev".into());
        let (delta, _) = initial_snapshots(device, Some(&changed), own, true);
        assert_eq!(
            types(&delta)
                .iter()
                .filter(|kind| *kind == PACKET_TYPE_SETTINGS)
                .count(),
            1
        );
        // This desk restarted: the phone's `have` names the old process.
        let (everything, _) = initial_snapshots(device, Some(&theirs), "desk-process-2", true);
        assert_eq!(
            types(&everything),
            types(&all),
            "a new epoch resends everything"
        );
        // A phone that takes no notifications gets none either way.
        let (without, _) = initial_snapshots(device, None, own, false);
        assert!(
            !types(&without).contains(&super::super::packet::PACKET_TYPE_NOTIFICATION.to_string())
        );
    }

    #[test]
    fn the_settings_and_art_packets_fit_one_wire_message() {
        let packet = settings_packet(&AppSettings::default(), &"d".repeat(64));
        assert!(crate::device_links::webrtc::fits_on_wire(&packet));
        let art = super::super::media_art::Art {
            id: "a".repeat(64),
            data: "A".repeat(super::super::media_art::MAX_JPEG_BYTES * 4 / 3 + 4),
        };
        let mut mpris = NetworkPacket::new(crate::device_links::packet::PACKET_TYPE_MPRIS);
        mpris.set("player", "\u{10348}".repeat(64));
        mpris.set("title", "\u{10348}".repeat(256));
        mpris.set("artist", "\u{10348}".repeat(256));
        mpris.set("album", "\u{10348}".repeat(256));
        super::super::mpris::add_art(&mut mpris, Some(&art), true);
        assert!(
            crate::device_links::webrtc::fits_on_wire(&mpris),
            "{:?}",
            crate::device_links::webrtc::enveloped_len(&mpris)
        );
    }

    #[test]
    fn the_settings_report_carries_every_key() {
        let mut settings = AppSettings::default();
        settings.hidden_notifications.insert("tablet".into());
        let packet = settings_packet(&settings, "tablet");
        for key in [
            "keepAwake",
            "clipboard",
            "notifications",
            "relayConfigured",
            "relay",
            "commandAlerts",
            "unlockNearby",
            "sessionPasswordSet",
        ] {
            assert!(packet.body.contains_key(key), "{key}");
        }
        assert_eq!(packet.get_bool("notifications"), Some(false), "per phone");
        assert_eq!(
            settings_packet(&settings, "phone").get_bool("notifications"),
            Some(true)
        );
        assert_eq!(packet.get_bool("unlockNearby"), Some(false));
    }

    #[test]
    fn only_allow_listed_settings_can_be_changed_from_the_phone() {
        let mut packet =
            NetworkPacket::new(crate::device_links::packet::PACKET_TYPE_SETTINGS_REQUEST);
        packet.set(
            "set",
            json!({
                "keepAwake": true,
                "clipboard": false,
                "notifications": false,
                "commandAlerts": true,
                "relay": true,
                "unlockNearby": true,
                "sessionPassword": false,
                "keepAwakeTypo": true,
                "clipboardNotBool": "yes",
            }),
        );
        let without_relay = parse_settings_request(&packet, false);
        assert_eq!(
            without_relay,
            vec![
                ("clipboard".to_string(), false),
                ("commandAlerts".to_string(), true),
                ("notifications".to_string(), false),
            ],
            "keepAwake, unlockNearby, unknown keys and relay without a server are ignored"
        );
        let with_relay = parse_settings_request(&packet, true);
        assert!(with_relay.contains(&("relay".to_string(), true)));
        assert!(!with_relay.iter().any(|(key, _)| key == "unlockNearby"));

        let mut settings = AppSettings::default();
        for (key, value) in with_relay {
            apply_change(&mut settings, "phone", &key, value);
        }
        assert!(!settings.stay_awake, "a phone cannot turn keep-awake on");
        assert!(!settings.clipboard_sync);
        assert!(!settings.shows_notifications("phone"));
        assert!(!settings.unlocks_from("phone"), "never from the phone");
    }

    #[test]
    fn unlock_and_alerts_are_per_phone() {
        let mut settings = AppSettings::default();
        settings.unlock_devices.insert("phone".into());
        assert_eq!(
            settings_packet(&settings, "phone").get_bool("unlockNearby"),
            Some(true)
        );
        assert_eq!(
            settings_packet(&settings, "tablet").get_bool("unlockNearby"),
            Some(false)
        );
        apply_change(&mut settings, "tablet", "commandAlerts", false);
        assert_eq!(
            settings_packet(&settings, "tablet").get_bool("commandAlerts"),
            Some(false)
        );
        assert_eq!(
            settings_packet(&settings, "phone").get_bool("commandAlerts"),
            Some(true)
        );

        let mut device = DeviceView::from_info(
            &crate::device_links::device_info::DeviceInfo {
                id: "tablet".into(),
                name: "Tab".into(),
                device_type: "tablet".into(),
                protocol_version: 9,
                incoming_capabilities: Vec::new(),
                outgoing_capabilities: Vec::new(),
                instance_id: None,
                app_version: None,
            },
            "10.0.0.3".into(),
            true,
        );
        device.status = DeviceStatus::Paired;
        device.feature_transport_ready = true;
        device.connection_path = Some("lan".into());
        device.connection_nearby = true;
        assert!(
            !could_unlock(&device, &settings),
            "allowed for another phone only"
        );
        settings.unlock_devices.insert("tablet".into());
        assert!(could_unlock(&device, &settings));
    }

    #[test]
    fn resent_settings_change_nothing() {
        let settings = AppSettings::default();
        let changes = vec![
            ("clipboard".to_string(), settings.clipboard_sync),
            ("notifications".to_string(), true),
            ("commandAlerts".to_string(), true),
            ("relay".to_string(), settings.relay_enabled),
        ];
        assert!(real_changes(&settings, "phone", changes).is_empty());
    }

    #[test]
    fn the_settings_packet_says_whether_a_session_password_is_set() {
        let mut settings = AppSettings::default();
        assert_eq!(
            settings_packet(&settings, "phone").get_bool("sessionPasswordSet"),
            Some(false)
        );
        settings.session_password =
            Some(crate::device_links::session_lock::hash_password("secret").unwrap());
        assert_eq!(
            settings_packet(&settings, "phone").get_bool("sessionPasswordSet"),
            Some(true)
        );
    }

    #[test]
    fn lock_and_hand_off_always_locks_first() {
        use std::cell::RefCell;
        let order = RefCell::new(Vec::<String>::new());
        let session = crate::device_links::pty::registry::SessionInfo {
            id: "t1".into(),
            title: "Shell".into(),
            rows: 24,
            cols: 80,
            exit_status: None,
            desk_window: false,
            cwd: Some("~/api".into()),
            started_at: 0,
            shell: None,
        };
        let phones = [("phone".to_string(), true), ("tablet".to_string(), false)];
        let record =
            |phone: &str, _: &NetworkPacket| order.borrow_mut().push(format!("send {phone}"));
        hand_off(
            || order.borrow_mut().push("lock".into()),
            Some(&session),
            &phones,
            record,
        )
        .unwrap();
        assert_eq!(
            *order.borrow(),
            ["lock", "send phone"],
            "only phones with terminal access"
        );

        order.borrow_mut().clear();
        assert!(hand_off(
            || order.borrow_mut().push("lock".into()),
            None,
            &phones,
            record
        )
        .is_err());
        assert_eq!(*order.borrow(), ["lock"], "locked even with no terminal");
        order.borrow_mut().clear();
        assert!(hand_off(
            || order.borrow_mut().push("lock".into()),
            Some(&session),
            &[],
            record
        )
        .is_err());
        assert_eq!(*order.borrow(), ["lock"], "locked even with no phone");
    }

    #[test]
    fn the_same_refusal_is_logged_once_per_window() {
        let now = Instant::now();
        let mut last = None;
        assert!(refusal_is_news(&mut last, "phone", Refusal::NotLan, now));
        assert!(!refusal_is_news(
            &mut last,
            "phone",
            Refusal::NotLan,
            now + Duration::from_secs(1)
        ));
        assert!(refusal_is_news(
            &mut last,
            "phone",
            Refusal::Expired,
            now + Duration::from_secs(2)
        ));
        assert!(refusal_is_news(
            &mut last,
            "tablet",
            Refusal::Expired,
            now + Duration::from_secs(3)
        ));
        assert!(refusal_is_news(
            &mut last,
            "tablet",
            Refusal::Expired,
            now + Duration::from_secs(3) + REFUSAL_LOG_WINDOW
        ));
    }

    #[test]
    fn a_flood_of_requests_waits_on_one_thread_and_the_rest_are_dropped() {
        let (started, release) = std::sync::mpsc::channel::<()>();
        let gate = Arc::new(Mutex::new(()));
        let held = gate.lock().unwrap();
        let blocker = Arc::clone(&gate);
        assert!(run_later(move || {
            let _ = started.send(());
            let _unused = blocker.lock();
        }));
        release.recv().unwrap();
        let before = dropped_jobs();
        let accepted = (0..QUEUE * 4).filter(|_| run_later(|| {})).count();
        assert!(accepted <= QUEUE, "{accepted} queued");
        // A full queue is visible: counted, and said in plain words.
        assert_eq!(dropped_jobs() - before, (QUEUE * 4 - accepted) as u64);
        assert_eq!(
            drop_notice(1),
            "DeskLink was busy and ignored a request from a phone. Try again."
        );
        assert!(drop_notice(5).contains("5 requests"));
        // Review finding F: work the sync cannot do without is waited for,
        // not dropped, while the queue is full.
        let (ran_tx, ran) = std::sync::mpsc::channel::<&'static str>();
        run_later_reliably(move || {
            let _ = ran_tx.send("ran");
        });
        assert!(
            ran.recv_timeout(Duration::from_millis(200)).is_err(),
            "still waiting for room"
        );
        drop(held);
        assert_eq!(
            ran.recv_timeout(Duration::from_secs(5)),
            Ok("ran"),
            "it ran once the queue drained"
        );
    }

    /// Review finding F: one job that panics must not end the desk thread.
    #[test]
    fn a_panicking_job_does_not_end_the_desk_thread() {
        let (done_tx, done) = std::sync::mpsc::channel::<u8>();
        run_later_reliably(|| panic!("a job failed"));
        run_later_reliably(move || {
            let _ = done_tx.send(1);
        });
        assert_eq!(done.recv_timeout(Duration::from_secs(10)), Ok(1));
    }

    /// Review finding H: a `have` that arrives after its connection ended is
    /// not kept for the next one.
    #[test]
    fn a_have_from_an_ended_connection_is_ignored() {
        let device = "stale-have-phone";
        super::super::sync::forget_device(device);
        let mut have = NetworkPacket::new(super::super::packet::PACKET_TYPE_SYNC_HAVE);
        have.set("peerInstance", "desk");
        have.set("have", serde_json::json!({"settings": "r1"}));
        let first = Some("attempt-1".to_string());
        let second = Some("attempt-2".to_string());
        // Queued on attempt 1, running when attempt 2 is current.
        assert!(!apply_have(device, &have, &first, &second));
        assert!(super::super::sync::theirs(device).is_none());
        // On its own connection it is kept.
        assert!(
            !apply_have(device, &have, &second, &second),
            "nothing waited"
        );
        assert!(super::super::sync::theirs(device).is_some());
        super::super::sync::forget_device(device);
    }

    #[test]
    fn a_handoff_names_the_terminal_and_its_folder() {
        let packet = handoff_packet("t1", "Shell", "~/api");
        assert_eq!(packet.packet_type, "desklink.handoff");
        assert_eq!(packet.get_str("sessionId"), Some("t1"));
        assert_eq!(packet.get_str("path"), Some("~/api"));
    }
}
