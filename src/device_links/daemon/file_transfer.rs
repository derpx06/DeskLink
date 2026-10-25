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

/// Files chosen for a phone that is not connected: kept (bounded) and sent
/// when it is (I9). The row shows "Waiting for <phone>" meanwhile.
const MAX_WAITING_FILES: usize = 200;
const MAX_WAITING_AGE: std::time::Duration = std::time::Duration::from_secs(24 * 3600);

struct Waiting {
    device_id: String,
    path: PathBuf,
    since: std::time::Instant,
    preparing: Preparing,
}

static WAITING: Mutex<Vec<Waiting>> = Mutex::new(Vec::new());

/// Keeps `path` for `device_id` until it connects. `Err` is a message for the
/// person.
fn wait_for_phone(device_id: &str, phone: &str, path: PathBuf) -> Result<(), String> {
    let meta = std::fs::metadata(&path).map_err(|_| "That file can't be read".to_string())?;
    if !meta.is_file() {
        return Err("Only files can be sent".to_string());
    }
    let mut waiting = WAITING.lock().map_err(|_| "Busy, try again".to_string())?;
    if waiting.len() >= MAX_WAITING_FILES {
        return Err(format!(
            "{MAX_WAITING_FILES} files are already waiting for a phone to connect"
        ));
    }
    let name = path.file_name().map_or_else(
        || "file".to_string(),
        |name| name.to_string_lossy().into_owned(),
    );
    let preparing = Preparing::new(device_id, &name);
    crate::device_links::webrtc::transfer_board::set_note(
        preparing.id(),
        Some(&format!("Waiting for {phone}")),
    );
    waiting.push(Waiting {
        device_id: device_id.to_string(),
        path,
        since: std::time::Instant::now(),
        preparing,
    });
    Ok(())
}

/// An unpaired phone's waiting files are forgotten with it.
pub(crate) fn forget_waiting(device_id: &str) {
    if let Ok(mut waiting) = WAITING.lock() {
        waiting.retain(|file| file.device_id != device_id);
    }
}

/// The waiting files of phones that are connected now, oldest first; files
/// that waited longer than a day are dropped (their rows go).
fn take_waiting(ready: &[String], now: std::time::Instant) -> Vec<Waiting> {
    let Ok(mut waiting) = WAITING.lock() else {
        return Vec::new();
    };
    waiting.retain(|file| now.saturating_duration_since(file.since) < MAX_WAITING_AGE);
    let (go, stay): (Vec<_>, Vec<_>) = std::mem::take(&mut *waiting)
        .into_iter()
        .partition(|file| ready.contains(&file.device_id));
    *waiting = stay;
    go
}

/// Sends what was waiting for the phones in `ready` (called every second by
/// the status reconciler).
pub(super) fn flush_waiting(
    sessions: &Arc<crate::device_links::core::DeviceManager>,
    errors: &Arc<Mutex<Vec<String>>>,
    ready: &[String],
) {
    if ready.is_empty() {
        return;
    }
    for file in take_waiting(ready, std::time::Instant::now()) {
        let Waiting {
            device_id,
            path,
            preparing,
            ..
        } = file;
        queue_send(sessions, errors, &device_id, path, Some(preparing));
    }
}

/// Queues `path` with the transfer manager of `device_id`'s ready transport.
fn queue_send(
    sessions: &Arc<crate::device_links::core::DeviceManager>,
    errors: &Arc<Mutex<Vec<String>>>,
    device_id: &str,
    file_path: PathBuf,
    waiting_row: Option<Preparing>,
) {
    let transport = match FeatureTransport::for_device(sessions, device_id) {
        Ok(transport) if transport.is_ready() => transport,
        Ok(_) | Err(_) => {
            debug_log!("[Daemon] File transfer for {device_id} was not started: not connected");
            push_error(
                errors,
                "That phone isn't connected, so the file was not sent".to_string(),
            );
            return;
        }
    };
    if QUEUED.fetch_add(1, Ordering::AcqRel) >= MAX_QUEUED {
        QUEUED.fetch_sub(1, Ordering::AcqRel);
        push_error(
            errors,
            format!("DeskLink is preparing {MAX_QUEUED} files already; send the rest in a moment"),
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
        errors: Arc::clone(errors),
        preparing: waiting_row.unwrap_or_else(|| Preparing::new(device_id, &name)),
    };
    if preparer().send(job).is_err() {
        QUEUED.fetch_sub(1, Ordering::AcqRel);
        push_error(
            errors,
            "DeskLink could not queue the file transfer".to_string(),
        );
    }
}

impl DaemonWorker {
    pub(super) fn send_file(&self, device_id: &str, file_path: PathBuf) {
        let ready = FeatureTransport::for_device(&self.sessions, device_id)
            .is_ok_and(|transport| transport.is_ready());
        if !ready {
            let paired = self
                .config
                .lock()
                .is_ok_and(|config| config.is_trusted(device_id));
            if !paired {
                push_error(&self.errors, "That phone isn't paired".to_string());
                return;
            }
            let phone = self
                .devices
                .lock()
                .ok()
                .and_then(|devices| devices.get(device_id).map(|device| device.name.clone()))
                .unwrap_or_else(|| "the phone".to_string());
            if let Err(message) = wait_for_phone(device_id, &phone, file_path) {
                push_error(&self.errors, message);
            }
            return;
        }
        queue_send(&self.sessions, &self.errors, device_id, file_path, None);
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
            // Paused or offline: there is no one to tell, and the row must
            // still go (it used to stay for ever, with a raw error) (I7).
            debug_log!("[Daemon] Cancelled locally, the phone was not told: {error}");
            crate::device_links::webrtc::transfer_board::finish(
                transfer_id,
                crate::device_links::webrtc::transfer_board::Outcome::Cancelled,
            );
        }
    }
}

#[cfg(test)]
mod waiting_tests {
    use super::*;

    /// I9: a file chosen while the phone is away is kept and goes out when it
    /// connects; the row says who it waits for.
    #[test]
    fn a_file_for_an_absent_phone_waits_and_is_released_when_it_connects() {
        let dir = std::env::temp_dir().join(format!("desklink-wait-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.txt");
        std::fs::write(&file, "x").unwrap();
        wait_for_phone("wait-phone", "Pixel", file.clone()).unwrap();
        assert!(wait_for_phone("wait-phone", "Pixel", dir.join("missing")).is_err());
        let rows = crate::device_links::webrtc::transfer_board::rows();
        let row = rows
            .iter()
            .find(|row| row.device_id == "wait-phone")
            .unwrap();
        assert_eq!(
            crate::device_links::webrtc::transfer_board::note(&row.id).as_deref(),
            Some("Waiting for Pixel")
        );
        let now = std::time::Instant::now();
        assert!(take_waiting(&["other".to_string()], now).is_empty());
        let go = take_waiting(&["wait-phone".to_string()], now);
        assert_eq!(go.len(), 1);
        assert_eq!(go[0].path, file);
        drop(go);
        assert!(!crate::device_links::webrtc::transfer_board::rows()
            .iter()
            .any(|row| row.device_id == "wait-phone"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn files_that_waited_a_day_are_dropped() {
        let dir = std::env::temp_dir().join(format!("desklink-wait-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("b.txt");
        std::fs::write(&file, "x").unwrap();
        wait_for_phone("old-phone", "Pixel", file).unwrap();
        let later = std::time::Instant::now() + MAX_WAITING_AGE;
        assert!(take_waiting(&["old-phone".to_string()], later).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
