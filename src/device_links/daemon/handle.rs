use std::collections::HashMap;
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread;

use super::commands::DaemonCommand;
use super::state::push_error;
use super::DaemonWorker;
use crate::device_links::device::DeviceView;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DaemonEvent {
    PingReceived {
        device_id: String,
        device_name: String,
        message: Option<String>,
    },
    FindPhoneReceived {
        device_id: String,
        device_name: String,
    },
}

struct DaemonStopGuard(Sender<DaemonCommand>);

impl Drop for DaemonStopGuard {
    fn drop(&mut self) {
        let _ = self.0.send(DaemonCommand::Stop);
    }
}

impl std::fmt::Debug for DaemonStopGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DaemonStopGuard")
    }
}

#[derive(Clone, Debug)]
pub struct DaemonHandle {
    command_tx: Sender<DaemonCommand>,
    _stop_guard: Arc<DaemonStopGuard>,
    devices: Arc<Mutex<HashMap<String, DeviceView>>>,
    errors: Arc<Mutex<Vec<String>>>,
    events: Arc<Mutex<Vec<DaemonEvent>>>,
}

impl DaemonHandle {
    pub fn start() -> Self {
        let (command_tx, command_rx) = mpsc::channel();
        let devices = Arc::new(Mutex::new(HashMap::new()));
        let errors = Arc::new(Mutex::new(Vec::new()));
        let events = Arc::new(Mutex::new(Vec::new()));
        let worker_devices = Arc::clone(&devices);
        let worker_errors = Arc::clone(&errors);
        let worker_events = Arc::clone(&events);

        thread::spawn(move || {
            if let Err(error) = DaemonWorker::new(
                worker_devices,
                worker_errors.clone(),
                worker_events,
                command_rx,
            )
            .and_then(DaemonWorker::run)
            {
                push_error(&worker_errors, error);
            }
        });

        let stop_guard = Arc::new(DaemonStopGuard(command_tx.clone()));

        Self {
            command_tx,
            _stop_guard: stop_guard,
            devices,
            errors,
            events,
        }
    }

    pub fn devices(&self) -> Vec<DeviceView> {
        let mut devices: Vec<_> = self
            .devices
            .lock()
            .map(|devices| devices.values().cloned().collect())
            .unwrap_or_default();
        devices.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        devices
    }

    pub fn drain_errors(&self) -> Vec<String> {
        self.errors
            .lock()
            .map(|mut errors| errors.drain(..).collect())
            .unwrap_or_default()
    }

    pub fn send(&self, command: DaemonCommand) {
        let _ = self.command_tx.send(command);
    }

    pub fn drain_events(&self) -> Vec<DaemonEvent> {
        self.events
            .lock()
            .map(|mut events| events.drain(..).collect())
            .unwrap_or_default()
    }
}
