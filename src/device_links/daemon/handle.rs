use std::sync::mpsc::{self, Sender};
use std::sync::Arc;
use std::thread;

use super::commands::DaemonCommand;
use super::state::push_error;
use super::DaemonWorker;
use crate::device_links::core::events::CoreEvent;
use crate::device_links::core::service::CoreService;
use crate::device_links::device::DeviceView;

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
    core: CoreService,
}

impl DaemonHandle {
    pub fn start() -> Self {
        let (command_tx, command_rx) = mpsc::channel();
        let core = CoreService::new();
        let devices = core.devices_storage();
        let errors = core.errors_storage();
        let events = core.events();
        let worker_devices = Arc::clone(&devices);
        let worker_errors = Arc::clone(&errors);
        let worker_events = events.clone();

        thread::spawn(move || {
            if let Err(error) = DaemonWorker::new(
                worker_devices,
                worker_errors.clone(),
                command_rx,
                worker_events.clone(),
            )
            .and_then(DaemonWorker::run)
            {
                push_error(&worker_errors, error);
                worker_events.publish(CoreEvent::Error {
                    scope: "daemon".to_string(),
                    device_id: None,
                    message: "The DeskLink daemon stopped unexpectedly".to_string(),
                    retryable: true,
                });
            }
        });

        let stop_guard = Arc::new(DaemonStopGuard(command_tx.clone()));

        Self {
            command_tx,
            _stop_guard: stop_guard,
            core,
        }
    }

    pub fn devices(&self) -> Vec<DeviceView> {
        self.core.snapshot()
    }

    pub fn drain_errors(&self) -> Vec<String> {
        self.core.drain_errors()
    }

    pub fn send(&self, command: DaemonCommand) {
        let _ = self.command_tx.send(command);
    }

    pub fn try_send(&self, command: DaemonCommand) -> bool {
        self.command_tx.send(command).is_ok()
    }

    pub fn subscribe_events(&self) -> futures::channel::mpsc::UnboundedReceiver<CoreEvent> {
        self.core.subscribe()
    }

    pub fn report_error(
        &self,
        scope: impl Into<String>,
        device_id: Option<String>,
        message: impl Into<String>,
        retryable: bool,
    ) {
        self.core.publish(CoreEvent::Error {
            scope: scope.into(),
            device_id,
            message: message.into(),
            retryable,
        });
    }
}
