use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex, OnceLock};

use super::state::push_error;
use super::DaemonWorker;
use crate::device_links::transport::FeatureTransport;
use crate::device_links::webrtc::transfer_board::Preparing;

/// Files waiting to be queued with the transfer manager, or being queued. Each
/// holds a row in the transfers window until the manager lists it itself.
const MAX_QUEUED: usize = 64;

static QUEUED: AtomicUsize = AtomicUsize::new(0);

struct SendJob {
    transport: FeatureTransport,
    path: PathBuf,
    device_id: String,
    errors: Arc<Mutex<Vec<String>>>,
    /// Shows the file as "preparing" from the moment it is queued until its
    /// turn ends.
    preparing: Preparing,
}

/// One thread queues files one at a time. A file is no longer copied or hashed
/// before its offer (the manager reads it in place, block by block), so this is
/// quick; it stays off the thread that answers pairing and pings because it
/// still touches the disk and the session.
fn preparer() -> &'static Sender<SendJob> {
    static PREPARER: OnceLock<Sender<SendJob>> = OnceLock::new();
    PREPARER.get_or_init(|| {
        let (queue, jobs) = channel::<SendJob>();
        let spawned = std::thread::Builder::new()
            .name("desklink-file-prepare".to_string())
            .spawn(move || {
                for job in jobs {
                    let SendJob {
                        transport,
                        path,
                        device_id,
                        errors,
                        preparing,
                    } = job;
                    if let Err(error) = transport.send_file(&path) {
                        let message = format!(
                            "DeskLink WebRTC file transfer for {device_id} failed: {error}"
                        );
                        debug_log!("[Daemon] {message}");
                        push_error(&errors, message);
                    }
                    // Whatever happened, the placeholder goes; a transfer that
                    // started has its own row and a failed one an error.
                    drop(preparing);
                    QUEUED.fetch_sub(1, Ordering::AcqRel);
                }
            });
        if let Err(error) = spawned {
            eprintln!("[Daemon] Could not start the file preparation thread: {error}");
        }
        queue
    })
}

impl DaemonWorker {
    pub(super) fn send_file(&self, device_id: &str, file_path: PathBuf) {
        let transport = match FeatureTransport::for_device(&self.sessions, device_id) {
            Ok(transport) => transport,
            Err(error) => {
                let message =
                    format!("DeskLink file transfer for {device_id} was not started: {error}");
                debug_log!("[Daemon] {message}");
                push_error(&self.errors, message);
                return;
            }
        };
        if !transport.is_ready() {
            let message = format!(
                "DeskLink file transfer for {device_id} requires an authenticated WebRTC file channel"
            );
            debug_log!("[Daemon] {message}");
            push_error(&self.errors, message);
            return;
        }
        if QUEUED.fetch_add(1, Ordering::AcqRel) >= MAX_QUEUED {
            QUEUED.fetch_sub(1, Ordering::AcqRel);
            push_error(
                &self.errors,
                "DeskLink is still preparing other files; try again in a moment".to_string(),
            );
            return;
        }
        let name = file_path.file_name().map_or_else(
            || "file".to_string(),
            |name| name.to_string_lossy().into_owned(),
        );
        let job = SendJob {
            transport,
            path: file_path,
            device_id: device_id.to_string(),
            errors: Arc::clone(&self.errors),
            preparing: Preparing::new(device_id, &name),
        };
        if preparer().send(job).is_err() {
            QUEUED.fetch_sub(1, Ordering::AcqRel);
            push_error(
                &self.errors,
                "DeskLink could not queue the file transfer".to_string(),
            );
        }
    }

    pub(super) fn cancel_transfer(&self, device_id: &str, transfer_id: &str) {
        let result =
            FeatureTransport::for_device(&self.sessions, device_id).and_then(|transport| {
                transport.cancel_transfer(
                    transfer_id,
                    crate::device_links::webrtc::file_protocol::code::USER,
                )
            });
        if let Err(error) = result {
            let message = format!("Could not cancel the file transfer: {error}");
            debug_log!("[Daemon] {message}");
            push_error(&self.errors, message);
        }
    }
}
