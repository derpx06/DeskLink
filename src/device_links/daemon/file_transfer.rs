use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex, OnceLock};

use super::state::push_error;
use super::DaemonWorker;
use crate::device_links::transport::FeatureTransport;
use crate::device_links::webrtc::transfer_board::Preparing;

/// Files waiting to be queued with the transfer manager, or being queued. Each
/// holds a row in the transfers window until the manager lists it itself.
const MAX_QUEUED: usize = 500;

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
    phone: String,
    path: PathBuf,
    since: std::time::Instant,
}

static WAITING: Mutex<Vec<Waiting>> = Mutex::new(Vec::new());

/// The transfers page lists waiting files as ONE row per phone ("N files
/// waiting for <phone>"), not one each: 200 waiting files must not push live
/// transfers off the board.
fn waiting_row_id(device_id: &str) -> String {
    format!("waiting-{device_id}")
}

/// Brings the aggregate rows in line with the queue (called with the queue
/// lock held, after every change).
fn refresh_waiting_rows(waiting: &[Waiting], shown: &mut Vec<String>) {
    use crate::device_links::webrtc::transfer_board as board;
    let mut now_shown = Vec::new();
    for file in waiting {
        if now_shown.contains(&file.device_id) {
            continue;
        }
        let count = waiting
            .iter()
            .filter(|other| other.device_id == file.device_id)
            .count();
        board::begin(board::TransferRow {
            id: waiting_row_id(&file.device_id),
            device_id: file.device_id.clone(),
            name: match count {
                1 => format!("1 file waiting for {}", file.phone),
                count => format!("{count} files waiting for {}", file.phone),
            },
            incoming: false,
            done: 0,
            total: 0,
            preparing: true,
            paused: false,
        });
        now_shown.push(file.device_id.clone());
    }
    for gone in shown.iter().filter(|id| !now_shown.contains(id)) {
        board::discard(&waiting_row_id(gone));
    }
    *shown = now_shown;
}

static SHOWN_ROWS: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn refresh_locked(waiting: &[Waiting]) {
    if let Ok(mut shown) = SHOWN_ROWS.lock() {
        refresh_waiting_rows(waiting, &mut shown);
    }
}

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
    waiting.push(Waiting {
        device_id: device_id.to_string(),
        phone: phone.to_string(),
        path,
        since: std::time::Instant::now(),
    });
    refresh_locked(&waiting);
    Ok(())
}

#[cfg(test)]
pub(super) fn wait_for_phone_for_tests(device_id: &str, phone: &str, path: PathBuf) {
    wait_for_phone(device_id, phone, path).unwrap();
}

/// An unpaired phone's waiting files are forgotten with it.
pub(crate) fn forget_waiting(device_id: &str) {
    if let Ok(mut waiting) = WAITING.lock() {
        waiting.retain(|file| file.device_id != device_id);
        refresh_locked(&waiting);
    }
}

/// The files waiting for `device_id`, oldest first: what the aggregate row's
/// list shows, so each can be taken off.
pub(crate) fn waiting_files(device_id: &str) -> Vec<PathBuf> {
    WAITING.lock().map_or_else(
        |_| Vec::new(),
        |waiting| {
            waiting
                .iter()
                .filter(|file| file.device_id == device_id)
                .map(|file| file.path.clone())
                .collect()
        },
    )
}

/// Takes one waiting file off the queue (the person's choice). `true` when
/// it was there.
pub(crate) fn remove_waiting_file(device_id: &str, path: &Path) -> bool {
    let Ok(mut waiting) = WAITING.lock() else {
        return false;
    };
    let before = waiting.len();
    waiting.retain(|file| !(file.device_id == device_id && file.path == path));
    let removed = waiting.len() != before;
    if removed {
        refresh_locked(&waiting);
    }
    removed
}

/// "Cancel": every file waiting for `device_id` is dropped.
pub(crate) fn clear_waiting(device_id: &str) {
    forget_waiting(device_id);
}

/// Whether a board row is the aggregate "N files waiting for <phone>".
pub(crate) fn is_waiting_row(id: &str) -> bool {
    id.starts_with("waiting-")
}

/// A phone paired again under a new id replaces the old record: what waited
/// for the old id now waits for the new one.
pub(crate) fn move_waiting(old_id: &str, new_id: &str) {
    if let Ok(mut waiting) = WAITING.lock() {
        for file in waiting.iter_mut().filter(|file| file.device_id == old_id) {
            file.device_id = new_id.to_string();
        }
        refresh_locked(&waiting);
    }
}

/// Drops what waited a day. Run on every tick, whether or not a phone is
/// connected.
fn expire_waiting(now: std::time::Instant) {
    if let Ok(mut waiting) = WAITING.lock() {
        let before = waiting.len();
        waiting.retain(|file| now.saturating_duration_since(file.since) < MAX_WAITING_AGE);
        if waiting.len() != before {
            refresh_locked(&waiting);
        }
    }
}

/// The waiting files of phones that are connected now, oldest first. They
/// leave the queue only once taken here; a file the send then refuses is put
/// back by [`flush_waiting`].
fn take_waiting(ready: &[String], now: std::time::Instant) -> Vec<Waiting> {
    expire_waiting(now);
    let Ok(mut waiting) = WAITING.lock() else {
        return Vec::new();
    };
    let (go, stay): (Vec<_>, Vec<_>) = std::mem::take(&mut *waiting)
        .into_iter()
        .partition(|file| ready.contains(&file.device_id));
    *waiting = stay;
    refresh_locked(&waiting);
    go
}

fn put_back(files: Vec<Waiting>) {
    if files.is_empty() {
        return;
    }
    if let Ok(mut waiting) = WAITING.lock() {
        let mut restored = files;
        restored.append(&mut waiting);
        *waiting = restored;
        refresh_locked(&waiting);
    }
}

/// Sends what was waiting for the phones in `ready` (called every second by
/// the status reconciler; expiry runs even with nobody connected).
pub(super) fn flush_waiting(
    sessions: &Arc<crate::device_links::core::DeviceManager>,
    errors: &Arc<Mutex<Vec<String>>>,
    ready: &[String],
) {
    expire_waiting(std::time::Instant::now());
    if ready.is_empty() {
        return;
    }
    let mut refused = Vec::new();
    for file in take_waiting(ready, std::time::Instant::now()) {
        // A refusal (not connected after all, the queue full) keeps the file
        // waiting: it is dropped only by age or by the person.
        if !queue_send(sessions, errors, &file.device_id, file.path.clone(), true) {
            refused.push(file);
        }
    }
    put_back(refused);
}

/// Queues `path` with the transfer manager of `device_id`'s ready transport.
fn queue_send(
    sessions: &Arc<crate::device_links::core::DeviceManager>,
    errors: &Arc<Mutex<Vec<String>>>,
    device_id: &str,
    file_path: PathBuf,
    from_waiting: bool,
) -> bool {
    let transport = match FeatureTransport::for_device(sessions, device_id) {
        Ok(transport) if transport.is_ready() => transport,
        Ok(_) | Err(_) => {
            debug_log!("[Daemon] File transfer for {device_id} was not started: not connected");
            if !from_waiting {
                push_error(
                    errors,
                    "That phone isn't connected, so the file was not sent".to_string(),
                );
            }
            return false;
        }
    };
    if QUEUED.fetch_add(1, Ordering::AcqRel) >= MAX_QUEUED {
        QUEUED.fetch_sub(1, Ordering::AcqRel);
        if !from_waiting {
            push_error(
                errors,
                format!("Up to {MAX_QUEUED} files can be queued at once; send the rest when these have started"),
            );
        }
        return false;
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
        preparing: Preparing::new(device_id, &name),
    };
    if preparer().send(job).is_err() {
        QUEUED.fetch_sub(1, Ordering::AcqRel);
        push_error(
            errors,
            "DeskLink could not queue the file transfer".to_string(),
        );
        return false;
    }
    true
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
        queue_send(&self.sessions, &self.errors, device_id, file_path, false);
    }

    pub(super) fn cancel_transfer(&self, device_id: &str, transfer_id: &str) {
        // The aggregate "N files waiting" row: cancelling it drops the queue.
        if is_waiting_row(transfer_id) {
            clear_waiting(device_id);
            return;
        }
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
    use crate::device_links::webrtc::transfer_board as board;

    /// I9 + S3: files for an absent phone wait as ONE row; they are released
    /// only when taken, can be put back when refused, expire after a day even
    /// with nobody connected, and follow a phone paired again under a new id.
    #[test]
    fn waiting_files_are_one_row_survive_a_refusal_expire_and_follow_a_new_id() {
        let dir = std::env::temp_dir().join(format!("desklink-wait-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let make = |name: &str| {
            let file = dir.join(name);
            std::fs::write(&file, "x").unwrap();
            file
        };
        let (a, b) = (make("a.txt"), make("b.txt"));
        wait_for_phone("wait-phone", "Pixel", a.clone()).unwrap();
        wait_for_phone("wait-phone", "Pixel", b).unwrap();
        assert!(wait_for_phone("wait-phone", "Pixel", dir.join("missing")).is_err());
        let rows: Vec<_> = board::rows()
            .into_iter()
            .filter(|row| row.device_id == "wait-phone")
            .collect();
        assert_eq!(rows.len(), 1, "one aggregate row, not one per file");
        assert_eq!(rows[0].name, "2 files waiting for Pixel");

        let now = std::time::Instant::now();
        assert!(take_waiting(&["other".to_string()], now).is_empty());
        let go = take_waiting(&["wait-phone".to_string()], now);
        assert_eq!(go.len(), 2);
        assert_eq!(go[0].path, a, "oldest first");
        assert!(board::rows()
            .iter()
            .all(|row| row.device_id != "wait-phone"));
        // The send refused them (the cap, not connected after all): back.
        put_back(go);
        assert!(board::rows()
            .iter()
            .any(|row| row.name == "2 files waiting for Pixel"));

        // The person can take one off, or all.
        assert_eq!(
            waiting_files("wait-phone"),
            vec![a.clone(), dir.join("b.txt")]
        );
        assert!(remove_waiting_file("wait-phone", &a));
        assert!(!remove_waiting_file("wait-phone", &a), "once");
        assert_eq!(waiting_files("wait-phone").len(), 1);
        assert!(board::rows()
            .iter()
            .any(|row| row.name == "1 file waiting for Pixel"));
        wait_for_phone("wait-phone", "Pixel", a.clone()).unwrap();
        clear_waiting("wait-phone");
        assert!(waiting_files("wait-phone").is_empty());
        assert!(board::rows()
            .iter()
            .all(|row| row.device_id != "wait-phone"));
        wait_for_phone("wait-phone", "Pixel", a.clone()).unwrap();
        wait_for_phone("wait-phone", "Pixel", dir.join("b.txt")).unwrap();
        let go = take_waiting(&["wait-phone".to_string()], now);
        put_back(go);

        // A phone paired again under a new id inherits them.
        move_waiting("wait-phone", "wait-phone-new");
        assert!(take_waiting(&["wait-phone".to_string()], now).is_empty());
        let moved = take_waiting(&["wait-phone-new".to_string()], now);
        assert_eq!(moved.len(), 2);
        put_back(moved);

        // A day later they are gone, whoever is connected.
        expire_waiting(std::time::Instant::now() + MAX_WAITING_AGE);
        assert!(take_waiting(&["wait-phone-new".to_string()], now).is_empty());
        assert!(board::rows()
            .iter()
            .all(|row| row.device_id != "wait-phone-new"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
