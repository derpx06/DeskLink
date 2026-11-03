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
/// when it is (I9). The row shows "Waiting for <phone>" meanwhile. The queue
/// is kept in the state folder, so it survives a restart of DeskLink.
const MAX_WAITING_FILES: usize = 200;
const MAX_WAITING_AGE: std::time::Duration = std::time::Duration::from_secs(24 * 3600);

/// What is kept of a waiting file: where it is and how it was when it was
/// chosen. It is sent only if it still is that file.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct Waiting {
    #[serde(rename = "device")]
    device_id: String,
    phone: String,
    path: PathBuf,
    size: u64,
    /// Modification time, nanoseconds since the epoch.
    mtime_ns: u64,
    /// When it was chosen, seconds since the epoch.
    queued_at: u64,
    /// Which phone it was chosen for (a hash of its pairing certificate): a
    /// phone that comes back under the same id with another certificate is
    /// not that phone.
    #[serde(default)]
    identity: String,
}

/// The identity a waiting file is kept for: a hash of the pairing certificate.
pub(crate) fn identity_of(certificate_pem: &str) -> String {
    openssl::sha::sha256(certificate_pem.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[derive(serde::Serialize, serde::Deserialize)]
struct SavedQueue {
    version: u32,
    files: Vec<Waiting>,
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

fn mtime_ns(meta: &std::fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |since| since.as_nanos().min(u128::from(u64::MAX)) as u64)
}

/// `$XDG_STATE_HOME/desklink/send-queue.json`. Tests get a file of their own,
/// never the person's.
fn queue_path() -> Option<PathBuf> {
    if cfg!(test) {
        return Some(std::env::temp_dir().join(format!(
            "desklink-test-send-queue-{}.json",
            std::process::id()
        )));
    }
    Some(
        dirs::state_dir()
            .or_else(dirs::data_local_dir)?
            .join("desklink")
            .join("send-queue.json"),
    )
}

/// The queue saved at `path`, as of `now`: nothing when it is missing or
/// damaged (it is rewritten at the next change), and without what waited a
/// day or more or what is past the cap.
fn load_queue(path: &Path, now: u64) -> Vec<Waiting> {
    let Ok(bytes) = std::fs::read(path) else {
        return Vec::new();
    };
    let Ok(saved) = serde_json::from_slice::<SavedQueue>(&bytes) else {
        debug_log!("[Daemon] The send queue in {path:?} is unreadable and was ignored");
        return Vec::new();
    };
    if saved.version != 1 {
        debug_log!("[Daemon] The send queue is of another version and was ignored");
        return Vec::new();
    }
    saved
        .files
        .into_iter()
        .filter(|file| now.saturating_sub(file.queued_at) < MAX_WAITING_AGE.as_secs())
        .take(MAX_WAITING_FILES)
        .collect()
}

/// Writes the queue to `path` (owner only); an empty queue removes the file.
fn save_queue(path: &Path, files: &[Waiting]) {
    if files.is_empty() {
        let _ = std::fs::remove_file(path);
        return;
    }
    let saved = SavedQueue {
        version: 1,
        files: files.to_vec(),
    };
    let written = serde_json::to_vec(&saved)
        .map_err(|error| error.to_string())
        .and_then(|bytes| {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir).map_err(|error| error.to_string())?;
            }
            crate::device_links::config::write_private(path, &bytes)
        });
    if let Err(error) = written {
        debug_log!("[Daemon] The send queue could not be saved: {error}");
    }
}

/// Brings back, at start, what waited when DeskLink last ran, for phones
/// still paired (`trusted`).
pub(crate) fn restore_waiting(identity: impl Fn(&str) -> Option<String>) {
    if let Some(path) = queue_path() {
        restore_from(&path, unix_now(), identity);
    }
}

/// `identity(device)` is the identity of a phone still paired; a file waits
/// again only for the very phone it was chosen for.
fn restore_from(path: &Path, now: u64, identity: impl Fn(&str) -> Option<String>) {
    let restored: Vec<Waiting> = load_queue(path, now)
        .into_iter()
        .filter(|file| identity(&file.device_id).is_some_and(|now| now == file.identity))
        .collect();
    if let Ok(mut waiting) = WAITING.lock() {
        let room = MAX_WAITING_FILES.saturating_sub(waiting.len());
        waiting.extend(restored.into_iter().take(room));
        refresh_locked(&waiting);
    }
}

/// The files that are no longer what was chosen: gone, or another size or
/// time. Split from `files`, with a sentence for the person for each.
fn split_changed(files: Vec<Waiting>) -> (Vec<Waiting>, Vec<String>) {
    let mut kept = Vec::new();
    let mut messages = Vec::new();
    for file in files {
        let same = std::fs::metadata(&file.path)
            .is_ok_and(|meta| meta.len() == file.size && mtime_ns(&meta) == file.mtime_ns);
        if same {
            kept.push(file);
        } else {
            let name = file.path.file_name().map_or_else(
                || "A file".to_string(),
                |name| name.to_string_lossy().into_owned(),
            );
            messages.push(format!(
                "“{name}” changed or was moved after it was chosen, so it was not sent to {}",
                file.phone
            ));
        }
    }
    (kept, messages)
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
    if let Some(path) = queue_path() {
        save_queue(&path, waiting);
    }
}

/// Keeps `path` for `device_id` until it connects. `Err` is a message for the
/// person.
fn wait_for_phone(
    device_id: &str,
    phone: &str,
    identity: &str,
    path: PathBuf,
) -> Result<(), String> {
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
        size: meta.len(),
        mtime_ns: mtime_ns(&meta),
        queued_at: unix_now(),
        identity: identity.to_string(),
    });
    refresh_locked(&waiting);
    Ok(())
}

#[cfg(test)]
pub(crate) fn wait_for_phone_for_tests(device_id: &str, phone: &str, path: PathBuf) {
    wait_for_phone(device_id, phone, "", path).unwrap();
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

/// Drops what waited a day (`now` in seconds since the epoch). Run on every
/// tick, whether or not a phone is connected.
fn expire_waiting(now: u64) {
    if let Ok(mut waiting) = WAITING.lock() {
        let before = waiting.len();
        waiting.retain(|file| now.saturating_sub(file.queued_at) < MAX_WAITING_AGE.as_secs());
        if waiting.len() != before {
            refresh_locked(&waiting);
        }
    }
}

/// The waiting files of phones that are connected now, oldest first. They
/// leave the queue only once taken here; a file the send then refuses is put
/// back by [`flush_waiting`].
fn take_waiting(ready: &[String], now: u64) -> Vec<Waiting> {
    expire_waiting(now);
    let Ok(mut waiting) = WAITING.lock() else {
        return Vec::new();
    };
    let (go, stay): (Vec<_>, Vec<_>) = std::mem::take(&mut *waiting)
        .into_iter()
        .partition(|file| ready.contains(&file.device_id));
    *waiting = stay;
    // Called every second for connected phones: write only on a change.
    if !go.is_empty() {
        refresh_locked(&waiting);
    }
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
    expire_waiting(unix_now());
    if ready.is_empty() {
        return;
    }
    let mut refused = Vec::new();
    // A file that is no longer what was chosen (it may be days old, from
    // before a restart) is dropped, out loud, not sent.
    let (go, changed) = split_changed(take_waiting(ready, unix_now()));
    // Three at most are named; more are one sentence.
    if changed.len() > 3 {
        push_error(
            errors,
            format!(
                "{} waiting files changed or were moved after they were chosen, so they were not sent",
                changed.len()
            ),
        );
    } else {
        for message in changed {
            push_error(errors, message);
        }
    }
    for file in go {
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
            let identity = self
                .config
                .lock()
                .ok()
                .and_then(|config| config.trusted_certificate_pem(device_id))
                .map(|pem| identity_of(&pem))
                .unwrap_or_default();
            if let Err(message) = wait_for_phone(device_id, &phone, &identity, file_path) {
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
        wait_for_phone("wait-phone", "Pixel", "", a.clone()).unwrap();
        wait_for_phone("wait-phone", "Pixel", "", b).unwrap();
        assert!(wait_for_phone("wait-phone", "Pixel", "", dir.join("missing")).is_err());
        let rows: Vec<_> = board::rows()
            .into_iter()
            .filter(|row| row.device_id == "wait-phone")
            .collect();
        assert_eq!(rows.len(), 1, "one aggregate row, not one per file");
        assert_eq!(rows[0].name, "2 files waiting for Pixel");

        let now = unix_now();
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
        wait_for_phone("wait-phone", "Pixel", "", a.clone()).unwrap();
        clear_waiting("wait-phone");
        assert!(waiting_files("wait-phone").is_empty());
        assert!(board::rows()
            .iter()
            .all(|row| row.device_id != "wait-phone"));
        wait_for_phone("wait-phone", "Pixel", "", a.clone()).unwrap();
        wait_for_phone("wait-phone", "Pixel", "", dir.join("b.txt")).unwrap();
        let go = take_waiting(&["wait-phone".to_string()], now);
        put_back(go);

        // A phone paired again under a new id inherits them.
        move_waiting("wait-phone", "wait-phone-new");
        assert!(take_waiting(&["wait-phone".to_string()], now).is_empty());
        let moved = take_waiting(&["wait-phone-new".to_string()], now);
        assert_eq!(moved.len(), 2);
        put_back(moved);

        // A day later they are gone, whoever is connected.
        // (Only these two have waited that long: the queue is shared with
        // the tests running beside this one.)
        for file in WAITING.lock().unwrap().iter_mut() {
            if file.device_id == "wait-phone-new" {
                file.queued_at = unix_now() - MAX_WAITING_AGE.as_secs();
            }
        }
        expire_waiting(unix_now());
        assert!(take_waiting(&["wait-phone-new".to_string()], now).is_empty());
        assert!(board::rows()
            .iter()
            .all(|row| row.device_id != "wait-phone-new"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("desklink-queue-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn entry(device: &str, path: &Path, queued_at: u64) -> Waiting {
        let meta = std::fs::metadata(path).unwrap();
        Waiting {
            device_id: device.into(),
            phone: "Pixel".into(),
            path: path.to_path_buf(),
            size: meta.len(),
            mtime_ns: mtime_ns(&meta),
            queued_at,
            identity: "cert-a".into(),
        }
    }

    /// G14: the queue is written to the state folder (owner only), comes
    /// back whole, and a restart restores it for paired phones only.
    #[test]
    fn the_queue_survives_a_restart() {
        let dir = scratch("round");
        // Entries queued a moment ago, not in 1970: the tests beside this one
        // call expire_waiting(unix_now()) on the shared queue, which drops
        // anything a day old and so dropped these between the restore and the
        // assertion.
        let queued = unix_now().saturating_sub(10);
        let now = queued + 10;
        let file = dir.join("a.txt");
        std::fs::write(&file, "x").unwrap();
        let state = dir.join("state").join("send-queue.json");
        let files = vec![
            entry("restart-phone", &file, queued),
            entry("gone", &file, queued),
        ];
        save_queue(&state, &files);
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&state).unwrap().permissions().mode();
            assert_eq!(mode & 0o077, 0, "owner only, was {mode:o}");
        }
        assert_eq!(load_queue(&state, now), files, "round trip");

        // A restart: only the phone still paired gets its files back.
        restore_from(&state, now, |id| {
            (id == "restart-phone").then(|| "cert-a".to_string())
        });
        assert_eq!(waiting_files("restart-phone"), vec![file.clone()]);
        assert!(waiting_files("gone").is_empty());
        // And it can be cancelled like any other.
        clear_waiting("restart-phone");
        assert!(waiting_files("restart-phone").is_empty());

        // Emptied, the file goes.
        save_queue(&state, &[]);
        assert!(!state.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn what_waited_a_day_is_dropped_at_load_and_the_cap_holds() {
        let dir = scratch("expiry");
        let file = dir.join("a.txt");
        std::fs::write(&file, "x").unwrap();
        let day = MAX_WAITING_AGE.as_secs();
        let state = dir.join("send-queue.json");
        save_queue(
            &state,
            &[
                entry("p", &file, 10),
                entry("p", &file, 11),
                entry("p", &file, 10 + day),
            ],
        );
        let kept = load_queue(&state, 10 + day);
        assert_eq!(
            kept.iter().map(|file| file.queued_at).collect::<Vec<_>>(),
            vec![11, 10 + day],
            "10 has waited a full day, 11 not yet"
        );
        let many: Vec<Waiting> = (0..250).map(|_| entry("p", &file, 100)).collect();
        save_queue(&state, &many);
        assert_eq!(load_queue(&state, 100).len(), MAX_WAITING_FILES);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_damaged_queue_file_is_ignored_not_fatal() {
        let dir = scratch("corrupt");
        let state = dir.join("send-queue.json");
        for bytes in [
            &b"{\"version\":1,\"files\":[{\"device\":"[..],
            b"",
            b"\xff\xfe not json",
            b"[]",
        ] {
            std::fs::write(&state, bytes).unwrap();
            assert!(load_queue(&state, 1).is_empty());
            restore_from(&state, 1, |_| Some("cert-a".to_string()));
        }
        assert!(load_queue(&dir.join("missing.json"), 1).is_empty());
        // Another version is not read as this one.
        let file = dir.join("a.txt");
        std::fs::write(&file, "x").unwrap();
        save_queue(&state, &[entry("p", &file, 1)]);
        let text = std::fs::read_to_string(&state)
            .unwrap()
            .replace("\"version\":1", "\"version\":2");
        std::fs::write(&state, text).unwrap();
        assert!(load_queue(&state, 2).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// G14: at send time a file that is no longer the one chosen (changed,
    /// replaced, gone) is dropped with a sentence naming it; the rest go.
    #[test]
    fn a_changed_file_is_dropped_with_a_message_at_send_time() {
        let dir = scratch("changed");
        let (same, grown, touched, gone) = (
            dir.join("same.txt"),
            dir.join("grown.txt"),
            dir.join("touched.txt"),
            dir.join("gone.txt"),
        );
        for file in [&same, &grown, &touched, &gone] {
            std::fs::write(file, "x").unwrap();
        }
        let files: Vec<Waiting> = [&same, &grown, &touched, &gone]
            .into_iter()
            .map(|file| entry("p", file, 1))
            .collect();
        std::fs::write(&grown, "xy").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&touched)
            .unwrap()
            .set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(5))
            .unwrap();
        std::fs::remove_file(&gone).unwrap();
        let (kept, messages) = split_changed(files);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].path, same);
        assert_eq!(messages.len(), 3);
        for name in ["grown.txt", "touched.txt", "gone.txt"] {
            assert!(
                messages
                    .iter()
                    .any(|m| m.contains(name) && m.contains("not sent to Pixel")),
                "{name}: {messages:?}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// F7: a phone that comes back under the same id with another pairing
    /// certificate is not the phone the files were chosen for.
    #[test]
    fn files_are_not_restored_for_a_phone_with_another_certificate() {
        let dir = scratch("identity");
        let file = dir.join("a.txt");
        std::fs::write(&file, "x").unwrap();
        let state = dir.join("send-queue.json");
        // Queued a moment ago (not in 1970): a neighbouring test's expire_waiting(unix_now()) drops the old.
        let queued = unix_now().saturating_sub(10);
        let now = queued + 10;
        save_queue(&state, &[entry("reinstalled-phone", &file, queued)]);
        // Same id, a different certificate: nothing comes back.
        restore_from(&state, now, |_| Some("cert-b".to_string()));
        assert!(waiting_files("reinstalled-phone").is_empty());
        // Unpaired since: nothing either.
        restore_from(&state, now, |_| None);
        assert!(waiting_files("reinstalled-phone").is_empty());
        // The same certificate: back.
        restore_from(&state, now, |_| Some("cert-a".to_string()));
        assert_eq!(waiting_files("reinstalled-phone"), vec![file]);
        clear_waiting("reinstalled-phone");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
