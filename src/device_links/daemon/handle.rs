use std::collections::HashMap;
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread;

use super::commands::DaemonCommand;
use super::state::push_error;
use super::DaemonWorker;
use crate::device_links::config::terminal_access_allowed;
use crate::device_links::device::DeviceView;
use crate::device_links::webrtc::transfer_board::{self, Finished, TransferRow};
use crate::device_links::webrtc::{coordinator, terminal_bridge};

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
    /// A phone scanned the pairing QR code and is now paired. Nothing was
    /// asked at the desk, so this is the only sign the scan worked.
    PairedByQr {
        device_id: String,
        device_name: String,
    },
    /// The phone offered its screen unasked: open the Phone Screen window.
    PhoneScreenOffered { device_id: String },
    /// A call the phone missed (T1 `missed`).
    MissedCall {
        device_id: String,
        device_name: String,
        caller: Option<String>,
        number: Option<String>,
    },
    /// The phone's calls changed (a snapshot arrived): shown at once.
    CallChanged { device_id: String },
    /// Sound to or from the phone was refused or ended there (F1).
    SoundNotice { device_id: String, message: String },
    /// Insert from the phone (F3): what came back or why it did not.
    CaptureNotice { device_id: String, message: String },
    /// A QR code the phone scanned holds a Wi-Fi network: offer to join.
    WifiFromPhone {
        device_id: String,
        network: crate::device_links::webrtc::capture::Wifi,
    },
    /// The phone refused a call request, in the user's words.
    CallRefused { message: String },
    /// A new pairing replaced an older record of the same name that was
    /// offline and looked like the same phone reinstalled.
    PairingReplaced { name: String },
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

/// Debug builds: made-up devices every reader of [`DaemonHandle::devices`]
/// sees (the top-bar extension, GNOME media, search), for screenshots of
/// the GNOME integration without a phone.
#[cfg(debug_assertions)]
pub static DEBUG_SHELL_DEVICES: Mutex<Vec<DeviceView>> = Mutex::new(Vec::new());

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
            device.desktop_session = coordinator::desktop_session(&device.id);
            device.desktop_extended = coordinator::desktop_extended(&device.id);
            device.capture =
                crate::device_links::webrtc::capture::status(&device.id).map(|capture| {
                    crate::device_links::device::CaptureUse {
                        kind: capture.kind,
                        receiving: capture.receiving,
                    }
                });
            device.sound = match crate::device_links::webrtc::audio_session::status(&device.id) {
                (None, None) => None,
                (sending, hearing) => {
                    use crate::device_links::webrtc::audio_session::source_name;
                    Some(crate::device_links::device::SoundUse {
                        sending: sending
                            .filter(|session| session.live)
                            .map(|session| source_name(session.source).to_string()),
                        hearing_live: hearing.as_ref().is_some_and(|session| session.live),
                        hearing: hearing.map(|session| source_name(session.source).to_string()),
                    })
                }
            };
        }
        #[cfg(debug_assertions)]
        if let Ok(fake) = DEBUG_SHELL_DEVICES.lock() {
            devices.extend(fake.iter().cloned());
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

    /// Every file transfer in progress, for a progress view.
    pub fn transfer_rows(&self) -> Vec<TransferRow> {
        transfer_board::rows()
    }

    /// Transfers that ended since the last call; each is reported once.
    pub fn take_finished_transfers(&self) -> Vec<Finished> {
        transfer_board::take_finished()
    }

    pub fn drain_events(&self) -> Vec<DaemonEvent> {
        let mut events: Vec<DaemonEvent> = self
            .events
            .lock()
            .map(|mut events| events.drain(..).collect())
            .unwrap_or_default();
        events.extend(crate::device_links::webrtc::capture::take_notices());
        events
    }
}
