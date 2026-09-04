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
        thread::spawn(move || {
            while !shutdown.load(Ordering::Acquire) {
                // A pairing negotiation nobody answered is abandoned on the
                // same tick, so the prompt clears without waiting for the
                // device to go unreachable — and the peer is told, so it stops
                // waiting too.
                pairing_commands::expire_pairings(&sessions, &devices, unix_secs());
                state::reconcile_connection_status(&devices, &sessions.connection_snapshot());
                thread::sleep(std::time::Duration::from_millis(1000));
            }
        });
    }

    pub(super) fn run(self) -> Result<(), String> {
        self.start_udp_listener()?;
        self.start_broadcaster();
        self.start_clipboard_listener();
        self.start_status_reconciler();

        while let Ok(command) = self.command_rx.recv() {
            eprintln!("[Daemon] Received command: {:?}", command);
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
