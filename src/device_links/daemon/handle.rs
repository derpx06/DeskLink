use std::collections::HashMap;
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread;

use super::commands::DaemonCommand;
use super::state::push_error;
use super::DaemonWorker;
use crate::device_links::config::terminal_access_allowed;
use crate::device_links::device::DeviceView;
use crate::device_links::webrtc::file_browser::{self, BrowserRequest, BrowserResponse};
use crate::device_links::webrtc::terminal_bridge;
use crate::device_links::webrtc::transfer_board::{self, Finished, TransferRow};

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
        // Terminal consent is read at the moment of asking, from the same place
        // the gate reads it, rather than cached on the view where a missed
        // update would leave the switch showing something that is not enforced.
        for device in &mut devices {
            device.terminal_access = terminal_access_allowed(&device.id);
            device.terminal_sessions_attached = terminal_bridge::attached_count(&device.id);
        }
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

    /// Asks the phone for files. Returns the request's ID, which the matching
    /// entry from [`Self::take_phone_file_replies`] carries.
    ///
    /// The request is validated and registered here, on the caller's thread, so
    /// a bad name fails immediately instead of after a round trip.
    pub fn browse_phone(
        &self,
        device_id: &str,
        request: &BrowserRequest,
    ) -> Result<String, String> {
        let (request_id, payload) = file_browser::begin_request(device_id, request)?;
        self.send(DaemonCommand::SendPhoneFileRequest(
            device_id.to_string(),
            request_id.clone(),
            payload,
        ));
        Ok(request_id)
    }

    pub fn take_phone_file_replies(&self, device_id: &str) -> Vec<BrowserResponse> {
        file_browser::take_replies(device_id)
    }

    /// Every file transfer in progress, for a progress view.
    pub fn transfer_rows(&self) -> Vec<TransferRow> {
        transfer_board::rows()
    }

    /// Transfers that ended since the last call; each is reported once.
    pub fn take_finished_transfers(&self) -> Vec<Finished> {
        transfer_board::take_finished()
    }

    pub fn drain_events(&self) -> Vec<DaemonEvent> {
        self.events
            .lock()
            .map(|mut events| events.drain(..).collect())
            .unwrap_or_default()
    }
}
