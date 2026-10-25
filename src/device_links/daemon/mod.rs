use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::thread;

use super::config::Config;
use super::core::DeviceManager;
use super::device::DeviceView;

/// Wall-clock seconds, for the pairing deadline comparison.
fn unix_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
}

/// Probe a bootstrap link that has been silent this long.
///
/// Only silence is probed, so an active link never sends one of these.
pub(super) const HEARTBEAT_IDLE: std::time::Duration = std::time::Duration::from_secs(10);

/// Declare a peer gone after this much silence — three missed probes.
///
/// Sits deliberately below Android's 120s idle reaper, so the two sides agree
/// a link is dead at roughly the same moment instead of one of them quietly
/// tearing down a link the other still believes in. That disagreement is the
/// livelock: it has been observed in both directions, once with the desktop
/// discarding 170 of the phone's fresh sockets while holding a dead one, and
/// once with the phone refusing to dial a desktop that was idle and waiting.
pub(super) const HEARTBEAT_DEAD: std::time::Duration = std::time::Duration::from_secs(30);

/// [`HEARTBEAT_DEAD`] while the phone reports `dozing`: its OS has frozen it,
/// so it cannot answer, and dropping the link once per freeze cycle would
/// only churn it. The kernel TCP keepalive (30 s / 10 s / 3, answered by the
/// phone's kernel even while frozen) still finds a phone that truly vanished.
pub(super) const HEARTBEAT_DEAD_DOZING: std::time::Duration = std::time::Duration::from_secs(90);

/// The LAN link's silence budget for a phone that is (or is not) dozing.
pub(super) fn heartbeat_dead(dozing: bool) -> std::time::Duration {
    if dozing {
        HEARTBEAT_DEAD_DOZING
    } else {
        HEARTBEAT_DEAD
    }
}

/// Whether a DeskLink window is on screen: the route and round trip are then
/// read every second instead of every five.
static UI_VISIBLE: AtomicBool = AtomicBool::new(false);

/// The window says whether it is showing (each refresh, and when it is
/// shown or hidden).
pub(crate) fn set_ui_visible(visible: bool) {
    UI_VISIBLE.store(visible, Ordering::Relaxed);
}

/// Whether any phone is reachable now (set by the status reconciler).
static PHONES_LIVE: AtomicBool = AtomicBool::new(false);

/// How often the main-loop tickers look at the daemon: every second while a
/// window shows or a phone is reachable, every five seconds when there is
/// nothing to show and nobody to talk to (a change in who is reachable wakes
/// the window at once, so the slow tick is only a safety net).
pub(crate) fn tick_seconds() -> u32 {
    tick_for(
        UI_VISIBLE.load(Ordering::Relaxed),
        PHONES_LIVE.load(Ordering::Relaxed),
    )
}

pub(crate) fn tick_for(window_shown: bool, phone_live: bool) -> u32 {
    if window_shown || phone_live {
        1
    } else {
        IDLE_TICK_SECONDS
    }
}

/// The tick with nothing live.
pub(crate) const IDLE_TICK_SECONDS: u32 = 5;

/// Reads each connected device's ICE path into its view, logging changes.
/// Every few seconds, off the UI thread: reading stats waits on webrtcbin.
fn refresh_connection_paths(
    devices: &Arc<Mutex<HashMap<String, DeviceView>>>,
    sessions: &Arc<DeviceManager>,
) {
    let ids: Vec<String> = devices
        .lock()
        .map(|devices| devices.keys().cloned().collect())
        .unwrap_or_default();
    for id in ids {
        let reading = sessions
            .ready_peer(&id)
            .and_then(|(peer, via_relay)| peer.path(via_relay));
        let (path, rtt, nearby) = reading.map_or((None, None, false), |reading| {
            (Some(reading.label), reading.rtt, reading.nearby)
        });
        let changed = devices.lock().ok().and_then(|mut devices| {
            let device = devices.get_mut(&id)?;
            device.rtt_ms = rtt;
            device.connection_nearby = nearby;
            let path = path.map(str::to_string);
            (device.connection_path != path).then(|| {
                device.connection_path = path.clone();
                path
            })
        });
        if let Some(Some(path)) = changed {
            super::connection_log::record_with_path(
                &id,
                super::connection_log::ConnectionEvent::Route,
                Some(&path),
            );
        }
    }
}

/// One pass of the liveness contract: probe the quiet, bury the silent.
fn run_liveness_tick(
    sessions: &Arc<DeviceManager>,
    devices: &Arc<Mutex<HashMap<String, DeviceView>>>,
) {
    let dead_after = |device_id: &str| {
        heartbeat_dead(crate::device_links::webrtc::coordinator::is_dozing(
            device_id,
        ))
    };
    let tick = sessions.liveness_tick(std::time::Instant::now(), HEARTBEAT_IDLE, dead_after);

    for binding in tick.probe {
        let packet = super::packet::NetworkPacket::new(super::packet::PACKET_TYPE_HEARTBEAT);
        // Best effort: a send failure here is itself evidence, and the dead
        // branch below will act on it once the silence budget runs out.
        let _ = binding.link.send_packet(&packet);
    }

    for binding in tick.dead {
        eprintln!(
            "[Daemon] No traffic from {} in {}s; treating the link as dead",
            binding.device_id,
            dead_after(&binding.device_id).as_secs()
        );
        handshake::handle_disconnect(
            &binding,
            devices,
            sessions,
            "The peer stopped responding".to_string(),
        );
    }
}

pub use commands::DaemonCommand;
pub use handle::{DaemonEvent, DaemonHandle};
pub(crate) use packet_handler::{
    admit_lock_operation, is_desktop_locked as desktop_locked, lock_desktop, open_link_now,
};
pub(crate) use state::{push_error as push_user_error, push_event};

pub mod clipboard;
pub mod commands;
pub mod connector;
pub mod discovery;
pub mod dispatcher;
pub mod file_transfer;
pub mod handle;
pub mod handshake;
pub mod network;
pub mod packet_handler;
pub mod pairing_commands;
mod power;
pub mod state;
#[cfg(test)]
pub mod test_peer;
pub mod validation;

pub(super) struct DaemonWorker {
    pub(super) config: Arc<Mutex<Config>>,
    pub(super) devices: Arc<Mutex<HashMap<String, DeviceView>>>,
    pub(super) sessions: Arc<DeviceManager>,
    pub(super) shutdown: Arc<AtomicBool>,
    pub(super) errors: Arc<Mutex<Vec<String>>>,
    pub(super) events: Arc<Mutex<Vec<handle::DaemonEvent>>>,
    pub(super) command_rx: Receiver<DaemonCommand>,
    pub(super) tcp_port: u16,
}

impl DaemonWorker {
    pub(super) fn new(
        devices: Arc<Mutex<HashMap<String, DeviceView>>>,
        errors: Arc<Mutex<Vec<String>>>,
        events: Arc<Mutex<Vec<handle::DaemonEvent>>>,
        command_rx: Receiver<DaemonCommand>,
    ) -> Result<Self, String> {
        // What this computer must have for a link to work, said once and
        // plainly (not as "Can't reach the phone", six rungs later).
        if let Some(problem) = crate::device_links::webrtc::plugin_problem() {
            eprintln!("[DeskLink] {problem}");
            state::push_error(&errors, problem);
        }
        let mut loaded = Config::load()?;
        // A damaged settings file set aside, or a newer one kept read-only:
        // said in words, once.
        if let Some(notice) = loaded.take_load_notice() {
            state::push_error(&errors, notice);
        }
        let config = Arc::new(Mutex::new(loaded));
        if let Ok(config) = config.lock() {
            state::seed_trusted_devices(
                &devices,
                config.trusted_devices().map(|device| {
                    (
                        device.id.clone(),
                        device.name.clone(),
                        device.device_type.clone(),
                        device.protocol_version,
                    )
                }),
            );
        }
        let tcp_listener = network::bind_tcp_listener()?;
        let tcp_port = tcp_listener
            .local_addr()
            .map_err(|err| err.to_string())?
            .port();
        let sessions = Arc::new(DeviceManager::new());
        let shutdown = Arc::new(AtomicBool::new(false));
        // Blocking: a phone's connection is taken the moment it arrives, not
        // at the next 50 ms poll. The accept thread lives as long as the app.

        {
            let config = Arc::clone(&config);
            let devices = Arc::clone(&devices);
            let sessions = Arc::clone(&sessions);
            let errors = Arc::clone(&errors);
            let events = Arc::clone(&events);
            let shutdown = Arc::clone(&shutdown);
            thread::spawn(move || {
                connector::incoming_tcp_loop(
                    tcp_listener,
                    config,
                    devices,
                    sessions,
                    errors,
                    events,
                    shutdown,
                )
            });
        }

        if let Ok(config) = config.lock() {
            crate::device_links::contacts::install(config.contacts_dir());
            crate::device_links::notes::install(config.notes_dir());
            if !cfg!(test) {
                crate::device_links::folders::start(config.folders_dir());
            }
        }
        crate::device_links::desk_features::install(
            Arc::clone(&sessions),
            Arc::clone(&config),
            Arc::clone(&devices),
            Arc::clone(&errors),
        );
        if !cfg!(test) {
            if crate::device_links::config::app_settings().forward_notifications {
                crate::device_links::desk_notifications::start();
            }
            crate::device_links::mpris::start();
            crate::device_links::glide::start();
            crate::device_links::pty::jobs::start_watch();
        }

        Ok(Self {
            config,
            devices,
            sessions,
            shutdown,
            errors,
            events,
            command_rx,
            tcp_port,
        })
    }

    /// Keeps the device view's reachability honest.
    ///
    /// Connection status used to be written opportunistically from discovery,
    /// the handshake and each disconnect handler, so any missed path left a
    /// stale "Connected" on screen. This reconciles the whole view against the
    /// session manager once a second, which makes a dropped link visible even
    /// when no handler observed the drop.
    fn start_status_reconciler(&self) {
        let devices = Arc::clone(&self.devices);
        let sessions = Arc::clone(&self.sessions);
        let shutdown = Arc::clone(&self.shutdown);
        let config = Arc::clone(&self.config);
        let errors = Arc::clone(&self.errors);
        let events = Arc::clone(&self.events);
        thread::spawn(move || {
            let mut stay_awake = power::StayAwake::default();
            let mut tick: u32 = 0;
            let mut last_reachable: Vec<(String, bool)> = Vec::new();
            while !shutdown.load(Ordering::Acquire) {
                tick = tick.wrapping_add(1);
                // Every second while a window shows them, else every five.
                if tick % 5 == 0 || UI_VISIBLE.load(Ordering::Relaxed) {
                    refresh_connection_paths(&devices, &sessions);
                }
                let wanted = super::config::app_settings().stay_awake
                    && config
                        .lock()
                        .is_ok_and(|config| config.has_trusted_devices());
                if let Err(error) = stay_awake.apply(wanted) {
                    state::push_error(&errors, error);
                }
                // A pairing negotiation nobody answered is abandoned on the
                // same tick, so the prompt clears without waiting for the
                // device to go unreachable — and the peer is told, so it stops
                // waiting too.
                pairing_commands::expire_pairings(&sessions, &devices, unix_secs());
                // A phone that paired again under a new id may be the same
                // phone reinstalled: its old record is replaced only when it
                // is offline and looks like the same machine, and the user is
                // told.
                for candidate in super::config::take_replacement_candidates() {
                    pairing_commands::consider_replacement(
                        &sessions, &devices, &config, &events, &errors, candidate,
                    );
                }
                run_liveness_tick(&sessions, &devices);
                let connections = sessions.connection_snapshot();
                state::reconcile_connection_status(&devices, &connections);
                // Who is reachable: the tickers slow down when nobody is,
                // and speed up (and the window refreshes) the moment that
                // changes.
                let mut reachable: Vec<(String, bool)> = connections
                    .iter()
                    .filter(|connection| connection.is_reachable())
                    .map(|connection| {
                        (
                            connection.device_id.clone(),
                            connection.feature_transport_ready,
                        )
                    })
                    .collect();
                reachable.sort();
                PHONES_LIVE.store(!reachable.is_empty(), Ordering::Relaxed);
                if reachable != last_reachable {
                    last_reachable = reachable;
                    state::wake_ui();
                    state::nudge_tickers();
                }
                thread::sleep(std::time::Duration::from_millis(1000));
            }
        });
    }

    pub(super) fn run(self) -> Result<(), String> {
        self.start_udp_listener()?;
        self.start_broadcaster();
        self.start_redialer();
        self.start_mdns_advertiser();
        self.start_clipboard_listener();
        self.start_status_reconciler();
        // First run: start at login unless the user already decided.
        if super::config::app_settings().start_at_login.is_none() {
            self.set_start_at_login(true);
        }
        crate::device_links::relay_link::client::start(
            crate::device_links::relay_link::client::Context {
                config: Arc::clone(&self.config),
                devices: Arc::clone(&self.devices),
                sessions: Arc::clone(&self.sessions),
                events: Arc::clone(&self.events),
                shutdown: Arc::clone(&self.shutdown),
                relay_url: None,
            },
        );

        while let Ok(command) = self.command_rx.recv() {
            match &command {
                // Never log a password.
                DaemonCommand::SetSessionPassword(_) => {
                    debug_log!("[Daemon] Received command: SetSessionPassword(<redacted>)")
                }
                other => debug_log!("[Daemon] Received command: {other:?}"),
            }
            if self.handle_command(command) {
                break;
            }
        }
        self.shutdown.store(true, Ordering::Release);
        for link in self.sessions.terminate_all() {
            link.close();
        }
        Ok(())
    }
}
