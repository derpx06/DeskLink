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
        let path = sessions
            .ready_peer(&id)
            .and_then(|(peer, via_relay)| peer.path(via_relay));
        let changed = devices.lock().ok().and_then(|mut devices| {
            let device = devices.get_mut(&id)?;
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
    let tick = sessions.liveness_tick(std::time::Instant::now(), HEARTBEAT_IDLE, HEARTBEAT_DEAD);

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
            HEARTBEAT_DEAD.as_secs()
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
        let config = Arc::new(Mutex::new(Config::load()?));
        let tcp_listener = network::bind_tcp_listener()?;
        let tcp_port = tcp_listener
            .local_addr()
            .map_err(|err| err.to_string())?
            .port();
        let sessions = Arc::new(DeviceManager::new());
        let shutdown = Arc::new(AtomicBool::new(false));
        tcp_listener
            .set_nonblocking(true)
            .map_err(|err| err.to_string())?;

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
        thread::spawn(move || {
            let mut stay_awake = power::StayAwake::default();
            let mut tick: u32 = 0;
            while !shutdown.load(Ordering::Acquire) {
                tick = tick.wrapping_add(1);
                if tick % 5 == 0 {
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
                run_liveness_tick(&sessions, &devices);
                state::reconcile_connection_status(&devices, &sessions.connection_snapshot());
                thread::sleep(std::time::Duration::from_millis(1000));
            }
        });
    }

    pub(super) fn run(self) -> Result<(), String> {
        self.start_udp_listener()?;
        self.start_broadcaster();
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
