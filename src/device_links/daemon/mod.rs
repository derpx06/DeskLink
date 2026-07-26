use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::thread;

use super::config::Config;
use super::core::DeviceManager;
use super::device::DeviceView;

pub use commands::DaemonCommand;
pub use handle::DaemonHandle;

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
pub mod state;
pub mod validation;

pub(super) struct DaemonWorker {
    pub(super) config: Arc<Mutex<Config>>,
    pub(super) devices: Arc<Mutex<HashMap<String, DeviceView>>>,
    pub(super) sessions: Arc<DeviceManager>,
    pub(super) shutdown: Arc<AtomicBool>,
    pub(super) errors: Arc<Mutex<Vec<String>>>,
    pub(super) command_rx: Receiver<DaemonCommand>,
    pub(super) tcp_port: u16,
}

impl DaemonWorker {
    pub(super) fn new(
        devices: Arc<Mutex<HashMap<String, DeviceView>>>,
        errors: Arc<Mutex<Vec<String>>>,
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
            let shutdown = Arc::clone(&shutdown);
            thread::spawn(move || {
                connector::incoming_tcp_loop(
                    tcp_listener,
                    config,
                    devices,
                    sessions,
                    errors,
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
            command_rx,
            tcp_port,
        })
    }

    pub(super) fn run(self) -> Result<(), String> {
        self.start_udp_listener()?;
        self.start_broadcaster();
        self.start_clipboard_listener();

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
