use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use super::config::Config;
use super::device::DeviceView;
use super::device_info::DeviceInfo;
use crate::device_links::core::device_manager::DeviceManager;
use crate::device_links::core::events::{CoreEvent, EventBus};
pub(crate) use crate::device_links::core::session_link::Link;
use crate::device_links::core::transfer_manager::{TransferCheckpointStore, TransferManager};

pub use commands::DaemonCommand;
pub use handle::DaemonHandle;

pub mod clipboard;
pub mod commands;
pub mod connector;
pub mod discovery;
pub mod file_transfer;
pub mod handle;
pub mod handshake;
pub mod network;
pub mod packet_handler;
pub mod screen_stream;
pub mod state;
pub mod validation;

#[derive(Clone)]
pub(super) struct ReconnectTarget {
    pub(super) info: DeviceInfo,
    pub(super) address: std::net::SocketAddr,
}

pub(super) struct DaemonWorker {
    pub(super) config: Arc<Mutex<Config>>,
    pub(super) devices: Arc<Mutex<HashMap<String, DeviceView>>>,
    pub(super) sessions: DeviceManager,
    pub(super) shutdown: Arc<AtomicBool>,
    pub(super) errors: Arc<Mutex<Vec<String>>>,
    pub(super) reconnect_targets: Arc<Mutex<HashMap<String, ReconnectTarget>>>,
    pub(super) transfer_cancellations: Arc<Mutex<HashSet<String>>>,
    pub(super) transfer_manager: TransferManager,
    pub(super) transfer_store: TransferCheckpointStore,
    pub(super) events: EventBus,
    pub(super) command_rx: Receiver<DaemonCommand>,
    pub(super) tcp_port: u16,
}

impl DaemonWorker {
    pub(super) fn new(
        devices: Arc<Mutex<HashMap<String, DeviceView>>>,
        errors: Arc<Mutex<Vec<String>>>,
        command_rx: Receiver<DaemonCommand>,
        events: EventBus,
    ) -> Result<Self, String> {
        let config = Arc::new(Mutex::new(Config::load()?));
        let tcp_listener = network::bind_tcp_listener()?;
        let tcp_port = tcp_listener
            .local_addr()
            .map_err(|err| err.to_string())?
            .port();
        let sessions = DeviceManager::new();
        let shutdown = Arc::new(AtomicBool::new(false));
        let reconnect_targets = Arc::new(Mutex::new(HashMap::new()));
        let transfer_cancellations = Arc::new(Mutex::new(HashSet::new()));
        let transfer_store = TransferCheckpointStore::new(
            config
                .lock()
                .map_err(|_| "Config lock poisoned".to_string())?
                .transfer_state_dir(),
        )?;
        let transfer_manager = TransferManager::default();

        {
            let config = Arc::clone(&config);
            let devices = Arc::clone(&devices);
            let sessions = sessions.clone();
            let shutdown = Arc::clone(&shutdown);
            let errors = Arc::clone(&errors);
            let incoming_events = events.clone();
            let transfer_cancellations = Arc::clone(&transfer_cancellations);
            thread::spawn(move || {
                connector::incoming_tcp_loop(
                    tcp_listener,
                    config,
                    devices,
                    sessions,
                    shutdown,
                    transfer_cancellations,
                    errors,
                    incoming_events,
                )
            });
        }

        Ok(Self {
            config,
            devices,
            sessions,
            shutdown,
            errors,
            reconnect_targets,
            transfer_cancellations,
            transfer_manager,
            transfer_store,
            events,
            command_rx,
            tcp_port,
        })
    }

    pub(super) fn run(self) -> Result<(), String> {
        self.start_udp_listener()?;
        if let Err(error) = self.start_mdns() {
            // Broadcast discovery remains available when mDNS is unavailable
            // (for example on a restricted sandbox or a network without
            // multicast), but the failure must remain visible to the UI.
            state::push_error(&self.errors, format!("DeskLink mDNS unavailable: {error}"));
            self.events.publish(CoreEvent::Error {
                scope: "discovery".to_string(),
                device_id: None,
                message: format!("DeskLink mDNS unavailable: {error}"),
                retryable: true,
            });
        }
        self.start_broadcaster();
        self.start_reconnect_scheduler();
        self.start_clipboard_listener();
        self.start_battery_broadcaster();

        while let Ok(command) = self.command_rx.recv() {
            eprintln!("[Daemon] Received command: {:?}", command);
            self.handle_command(command);
            if self.shutdown.load(Ordering::SeqCst) {
                break;
            }
        }
        Ok(())
    }

    fn start_reconnect_scheduler(&self) {
        let targets = Arc::clone(&self.reconnect_targets);
        let config = Arc::clone(&self.config);
        let devices = Arc::clone(&self.devices);
        let sessions = self.sessions.clone();
        let shutdown = Arc::clone(&self.shutdown);
        let errors = Arc::clone(&self.errors);
        let events = self.events.clone();
        let transfer_cancellations = Arc::clone(&self.transfer_cancellations);

        thread::spawn(move || loop {
            if shutdown.load(Ordering::SeqCst) {
                break;
            }
            let now = Instant::now();
            let candidates = targets
                .lock()
                .ok()
                .map(|targets| {
                    targets
                        .values()
                        .map(|target| (target.info.clone(), target.address))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();

            for (info, address) in candidates {
                let Some(lease) = sessions.claim_reconnect(&info.id, now) else {
                    continue;
                };
                let result = validation::connect_to_device(
                    info.clone(),
                    address,
                    address.port(),
                    Arc::clone(&config),
                    Arc::clone(&devices),
                    sessions.clone(),
                    events.clone(),
                    Arc::clone(&transfer_cancellations),
                );

                if let Err(error) = result {
                    sessions.reconnect_failed(&lease, error.clone());
                    let next_attempts = sessions
                        .sessions_snapshot()
                        .into_iter()
                        .find(|session| session.device_id == info.id)
                        .map(|session| session.reconnect_attempt)
                        .unwrap_or_default();
                    state::mark_error(&devices, &info.id, error.clone());
                    state::push_error(
                        &errors,
                        format!("Reconnect to {} failed: {error}", info.name),
                    );
                    events.publish(CoreEvent::ConnectionChanged {
                        device_id: info.id.clone(),
                        state: crate::device_links::core::device_session::DeviceConnectionState::Reconnecting {
                            attempt: next_attempts,
                        },
                        message: Some(error),
                    });
                }
            }
            thread::sleep(Duration::from_millis(250));
        });
    }

    fn start_battery_broadcaster(&self) {
        let sessions = self.sessions.clone();
        let shutdown = Arc::clone(&self.shutdown);
        let errors = Arc::clone(&self.errors);
        let events = self.events.clone();
        thread::spawn(move || loop {
            if shutdown.load(Ordering::SeqCst) {
                break;
            }
            if let Ok(Some(packet)) = crate::device_links::plugins::battery::local_packet() {
                for session in sessions.sessions_snapshot() {
                    if session.pair_state != crate::device_links::pairing::PairState::Paired {
                        continue;
                    }
                    if let Some(binding) = sessions.current_binding(&session.device_id) {
                        if !sessions.is_current(&binding) {
                            continue;
                        }
                        if let Err(error) = network::send_packet(&binding.link.stream, &packet) {
                            state::push_error(
                                &errors,
                                format!("Battery update to {} failed: {error}", session.device_id),
                            );
                            events.publish(CoreEvent::Error {
                                scope: "battery".to_string(),
                                device_id: Some(session.device_id.clone()),
                                message: error,
                                retryable: true,
                            });
                        }
                    }
                }
            }
            thread::sleep(Duration::from_secs(30));
        });
    }
}
