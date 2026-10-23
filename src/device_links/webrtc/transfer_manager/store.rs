//! What the transfer manager keeps on disk, and the filesystem rules around
//! it: DeskLink's own state directory (partial files, block-hash sidecars,
//! checkpoints) and the downloads directory.
//!
//! Peer input never names a path here: every name is built from a validated
//! transfer id or a validated filename, and `checked_child` refuses anything
//! that is not a direct child of its root.

use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use super::super::file_protocol::{
    blocks_for, validate_filename, FolderRef, BLOCK_BYTES, MAX_TRANSFER_BYTES,
};

/// A transfer's files untouched this long belong to a transfer nobody is
/// going to finish. Live ones are rewritten at every checkpoint, so age is a
/// safe test.
pub const STALE_AFTER: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// What a receiver has durably verified of one incoming file. It never claims
/// more than the block-hash sidecar and the partial file prove (see
/// [`Store::load_receive`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct ReceiveCheckpoint {
    pub transfer_id: String,
    pub device_id: String,
    pub token: String,
    pub filename: String,
    pub destination_name: String,
    pub total_size: u64,
    pub mtime: i64,
    pub blocks_done: u64,
    #[serde(default)]
    pub clipboard: bool,
    #[serde(default)]
    pub capture_id: Option<String>,
    #[serde(default)]
    pub folder: Option<FolderRef>,
    #[serde(default)]
    pub content_sha256: Option<String>,
}

impl ReceiveCheckpoint {
    /// A checkpoint read back from disk is untrusted state: every field that
    /// names a file or bounds a write must still make sense for `transfer_id`.
    fn is_consistent(&self, transfer_id: &str) -> bool {
        self.transfer_id == transfer_id
            && self.total_size <= MAX_TRANSFER_BYTES
            && self.blocks_done <= blocks_for(self.total_size)
            && validate_filename(&self.filename).is_ok()
            && validate_filename(&self.destination_name).is_ok()
    }
}

/// A checkpoint and the hashes of the blocks it vouches for.
pub(super) type Held = (ReceiveCheckpoint, Vec<[u8; 32]>);

/// What a sender keeps so an interrupted send can be offered again under the
/// same id and token. The source is read in place, never copied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct SendCheckpoint {
    pub transfer_id: String,
    pub device_id: String,
    pub token: String,
    pub filename: String,
    pub total_size: u64,
    pub mtime: i64,
    pub source: PathBuf,
    /// The user's own pick: listed in the history when it completes.
    #[serde(default)]
    pub history: bool,
    #[serde(default)]
    pub clipboard_mime: Option<String>,
    #[serde(default)]
    pub folder: Option<FolderRef>,
    #[serde(default)]
    pub content_sha256: Option<String>,
    /// Times the source changed under this send and it began again.
    #[serde(default)]
    pub restarts: u32,
}

impl SendCheckpoint {
    fn is_consistent(&self, transfer_id: &str) -> bool {
        self.transfer_id == transfer_id
            && self.total_size <= MAX_TRANSFER_BYTES
            && validate_filename(&self.filename).is_ok()
            && self.source.is_absolute()
    }
}

/// The two roots and every path under them.
#[derive(Debug, Clone)]
pub(super) struct Store {
    state_root: PathBuf,
    destination_root: PathBuf,
}

impl Store {
    pub fn new(state_root: PathBuf, destination_root: PathBuf) -> Result<Self, String> {
        fs::create_dir_all(&state_root).map_err(|error| {
            format!("Could not create DeskLink transfer state directory: {error}")
        })?;
        fs::create_dir_all(&destination_root)
            .map_err(|error| format!("Could not create DeskLink download directory: {error}"))?;
        ensure_directory_not_symlink(&state_root)?;
        // A downloads folder that is itself a link (a common arrangement:
        // `~/Downloads` on another disk) is resolved once, here, and the real
        // directory is what files go into; the check against a link swapped
        // in later then applies to that real directory.
        let destination_root = fs::canonicalize(&destination_root)
            .map_err(|error| format!("Could not resolve DeskLink download directory: {error}"))?;
        ensure_directory_not_symlink(&destination_root)?;
        let store = Self {
            state_root,
            destination_root,
        };
        store.sweep_stale(STALE_AFTER);
        Ok(store)
    }

    /// Removes the files of transfers nobody has touched for `max_age`: a
    /// partial download of up to 4 GiB would otherwise sit on the disk for
    /// ever. Only files this manager names (`.desklink-…`) are looked at,
    /// and only regular ones.
    pub fn sweep_stale(&self, max_age: Duration) {
        let Some(cutoff) = SystemTime::now().checked_sub(max_age) else {
            return;
        };
        sweep_stages(&self.destination_root, cutoff);
        let Ok(entries) = fs::read_dir(&self.state_root) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let ours = name.starts_with(".desklink-")
                && [
                    ".send",
                    ".send.json",
                    ".part",
                    ".json",
                    ".tmp",
                    ".sync",
                    ".blocks",
                ]
                .iter()
                .any(|suffix| name.ends_with(suffix));
            if !ours {
                continue;
            }
            let stale = fs::symlink_metadata(entry.path())
                .ok()
                .filter(|metadata| metadata.is_file())
                .and_then(|metadata| metadata.modified().ok())
                .is_some_and(|modified| modified < cutoff);
            if stale {
                let _ = fs::remove_file(entry.path());
            }
        }
    }

    pub fn state_root(&self) -> &Path {
        &self.state_root
    }

    fn state_path(&self, name: &str) -> Result<PathBuf, String> {
        checked_child(&self.state_root, name)
    }

    pub fn destination_path(&self, name: &str) -> Result<PathBuf, String> {
        checked_child(&self.destination_root, name)
    }

    pub fn part_path(&self, id: &str) -> Result<PathBuf, String> {
        self.state_path(&format!(".desklink-{id}.part"))
    }

    pub fn stash_path(&self, id: &str) -> Result<PathBuf, String> {
        self.state_path(&format!(".desklink-{id}.sync"))
    }

    fn blocks_name(id: &str) -> String {
        format!(".desklink-{id}.blocks")
    }

    fn checkpoint_name(id: &str) -> String {
        format!(".desklink-{id}.json")
    }

    fn send_name(id: &str) -> String {
        format!(".desklink-{id}.send.json")
    }

    /// Writes `bytes` to `name` in the state directory so that a crash leaves
    /// the old content or the new, never half of it.
    fn write_atomic(&self, name: &str, bytes: &[u8]) -> Result<(), String> {
        let path = self.state_path(name)?;
        let temporary = self.state_path(&format!("{name}.tmp"))?;
        let mut file = create_fresh_file(&temporary)?;
        file.write_all(bytes)
            .map_err(|error| format!("Could not write DeskLink transfer state: {error}"))?;
        file.sync_all()
            .map_err(|error| format!("Could not persist DeskLink transfer state: {error}"))?;
        fs::rename(temporary, path).map_err(|error| {
            format!("Could not atomically update DeskLink transfer state: {error}")
        })?;
        sync_directory(&self.state_root)
    }

    fn read_regular(&self, name: &str) -> Result<Option<Vec<u8>>, String> {
        let path = self.state_path(name)?;
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_file() => fs::read(&path)
                .map(Some)
                .map_err(|error| format!("Could not read DeskLink transfer state: {error}")),
            // A link or directory is not something this manager wrote.
            Ok(_) => Ok(Some(Vec::new())),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
            Err(error) => Err(format!(
                "Could not inspect DeskLink transfer state: {error}"
            )),
        }
    }

    /// Records `blocks_done` verified blocks: the block-hash sidecar first,
    /// then the checkpoint, so the checkpoint never claims more than the
    /// sidecar proves. The caller has already synced the partial file.
    pub fn save_receive(
        &self,
        checkpoint: &ReceiveCheckpoint,
        hashes: &[[u8; 32]],
    ) -> Result<(), String> {
        let id = &checkpoint.transfer_id;
        self.write_atomic(&Self::blocks_name(id), &hashes.concat())?;
        let bytes = serde_json::to_vec(checkpoint).map_err(|error| {
            format!("Could not serialize DeskLink transfer checkpoint: {error}")
        })?;
        self.write_atomic(&Self::checkpoint_name(id), &bytes)
    }

    /// The checkpoint of `id` and the hashes of its verified blocks, lowered
    /// to what the sidecar and the partial file both support. One that cannot
    /// be parsed, or makes no sense, is as good as none: it is removed with
    /// the files nothing can vouch for any more, instead of wedging the id.
    pub fn load_receive(&self, id: &str) -> Result<Option<Held>, String> {
        let Some(bytes) = self.read_regular(&Self::checkpoint_name(id))? else {
            return Ok(None);
        };
        let checkpoint = match serde_json::from_slice::<ReceiveCheckpoint>(&bytes) {
            Ok(checkpoint) if checkpoint.is_consistent(id) => checkpoint,
            _ => {
                self.remove_receive(id)?;
                return Ok(None);
            }
        };
        let sidecar = self
            .read_regular(&Self::blocks_name(id))?
            .unwrap_or_default();
        let hashes: Vec<[u8; 32]> = sidecar
            .chunks_exact(32)
            .map(|chunk| {
                let mut hash = [0u8; 32];
                hash.copy_from_slice(chunk);
                hash
            })
            .collect();
        let part_length = match fs::symlink_metadata(self.part_path(id)?) {
            Ok(metadata) if metadata.is_file() => metadata.len(),
            _ => 0,
        };
        // Blocks the partial file really holds.
        let held = (0..=checkpoint.blocks_done.min(hashes.len() as u64))
            .rev()
            .find(|blocks| (blocks * BLOCK_BYTES).min(checkpoint.total_size) <= part_length)
            .unwrap_or(0);
        let mut checkpoint = checkpoint;
        checkpoint.blocks_done = held;
        let hashes = hashes[..held as usize].to_vec();
        Ok(Some((checkpoint, hashes)))
    }

    /// Everything of an incoming transfer: partial file, sidecar, checkpoint.
    pub fn remove_receive(&self, id: &str) -> Result<(), String> {
        remove_if_present(&self.part_path(id)?)?;
        remove_if_present(&self.state_path(&Self::blocks_name(id))?)?;
        remove_if_present(&self.state_path(&Self::checkpoint_name(id))?)?;
        remove_if_present(&self.state_path(&format!("{}.tmp", Self::checkpoint_name(id)))?)?;
        remove_if_present(&self.state_path(&format!("{}.tmp", Self::blocks_name(id)))?)?;
        sync_directory(&self.state_root)
    }

    /// A fresh, empty partial file for `id`, replacing whatever was at the
    /// path (a crash orphan or a planted link).
    pub fn create_part(&self, id: &str) -> Result<File, String> {
        let file = create_fresh_file(&self.part_path(id)?)?;
        file.sync_all()
            .map_err(|error| format!("Could not persist DeskLink partial file: {error}"))?;
        Ok(file)
    }

    /// The existing partial file of `id`, for reading and writing.
    pub fn open_part(&self, id: &str) -> Result<File, String> {
        let path = self.part_path(id)?;
        ensure_regular_file(&path)?;
        OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|error| format!("Could not open DeskLink partial file: {error}"))
    }

    pub fn save_send(&self, checkpoint: &SendCheckpoint) -> Result<(), String> {
        let bytes = serde_json::to_vec(checkpoint).map_err(|error| error.to_string())?;
        self.write_atomic(&Self::send_name(&checkpoint.transfer_id), &bytes)
    }

    pub fn remove_send(&self, id: &str) -> Result<(), String> {
        remove_if_present(&self.state_path(&Self::send_name(id))?)?;
        sync_directory(&self.state_root)
    }

    /// The sends left on disk, each one checked to be a well-formed record
    /// (a checkpoint is untrusted state once read back).
    pub fn stored_sends(&self) -> Vec<SendCheckpoint> {
        let Ok(entries) = fs::read_dir(&self.state_root) else {
            return Vec::new();
        };
        let mut found = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(id) = name
                .to_str()
                .and_then(|name| name.strip_prefix(".desklink-"))
                .and_then(|name| name.strip_suffix(".send.json"))
            else {
                continue;
            };
            let Ok(Some(bytes)) = self.read_regular(&Self::send_name(id)) else {
                continue;
            };
            match serde_json::from_slice::<SendCheckpoint>(&bytes) {
                Ok(checkpoint) if checkpoint.is_consistent(id) => found.push(checkpoint),
                _ => {
                    let _ = self.remove_send(id);
                }
            }
        }
        found
    }

    /// A name in the downloads directory that nothing occupies: `name.ext`,
    /// else `name (2).ext`, `name (3).ext`… Occupied means "any directory
    /// entry", so a dangling symlink counts, and a suffix never pushes the
    /// name past the 255-byte filesystem limit.
    pub fn available_destination_name(&self, filename: &str) -> Result<String, String> {
        let taken = |name: &str| -> Result<bool, String> {
            Ok(fs::symlink_metadata(self.destination_path(name)?).is_ok())
        };
        if !taken(filename)? {
            return Ok(filename.to_string());
        }
        let source = Path::new(filename);
        // A very long "extension" is just part of the name; keeping it would
        // leave no room for the suffix.
        let extension = source
            .extension()
            .and_then(|value| value.to_str())
            .filter(|value| !value.is_empty() && value.len() <= 32);
        let stem = match extension {
            Some(_) => source
                .file_stem()
                .and_then(|value| value.to_str())
                .unwrap_or("file"),
            None => filename,
        };
        for number in 2..10_000u32 {
            let suffix = match extension {
                Some(extension) => format!(" ({number}).{extension}"),
                None => format!(" ({number})"),
            };
            let name = format!("{}{suffix}", truncate_to_boundary(stem, 255 - suffix.len()));
            if !taken(&name)? {
                return Ok(name);
            }
        }
        Err("Could not allocate a collision-safe DeskLink download name".to_string())
    }

    /// Moves a finished partial file into the downloads directory under
    /// `name`, or the next free name if it was taken in the meantime. The
    /// hard link never replaces anything and the file appears whole.
    pub fn publish(&self, id: &str, name: &str, filename: &str) -> Result<PathBuf, String> {
        self.publish_with(id, name, filename, &|part, target| {
            fs::hard_link(part, target)
        })
    }

    /// [`publish`](Self::publish) with the way a file is linked into place
    /// given (tests make it fail the way another filesystem does).
    ///
    /// A failure leaves the partial file where it is: it is verified and a
    /// retry only has to place it.
    pub(super) fn publish_with(
        &self,
        id: &str,
        name: &str,
        filename: &str,
        link: &dyn Fn(&Path, &Path) -> std::io::Result<()>,
    ) -> Result<PathBuf, String> {
        let part = self.part_path(id)?;
        // The folder may have gone while the file arrived (moved, unmounted).
        fs::create_dir_all(&self.destination_root)
            .map_err(|error| plain_io("make the downloads folder", &error))?;
        ensure_directory_not_symlink(&self.destination_root)?;
        let mut name = name.to_string();
        for _ in 0..16 {
            let target = self.destination_path(&name)?;
            match place_file(&part, &target, &self.destination_root, id, link) {
                Ok(()) => {
                    // The download is already published; a leftover part
                    // file is only clutter, and a directory that will not
                    // sync does not unpublish it.
                    let _ = fs::remove_file(&part);
                    if let Err(error) = sync_directory(&self.destination_root) {
                        debug_log!("[DeskLink] {error}");
                    }
                    return Ok(target);
                }
                Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                    name = self.available_destination_name(filename)?;
                }
                Err(error) => return Err(plain_io("save the download", &error)),
            }
        }
        Err("Could not allocate a collision-safe DeskLink download name".to_string())
    }

    /// Bytes free on the filesystem of the state directory, if it can be told.
    pub fn free_bytes(&self) -> Option<u64> {
        use std::os::unix::ffi::OsStrExt;
        let path = std::ffi::CString::new(self.state_root.as_os_str().as_bytes()).ok()?;
        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
        // SAFETY: `path` is a valid C string and `stat` a valid out pointer.
        let status = unsafe { libc::statvfs(path.as_ptr(), &mut stat) };
        (status == 0).then(|| (stat.f_bavail as u64).saturating_mul(stat.f_frsize as u64))
    }
}

/// The hidden file a copy is staged in beside its target: one per transfer
/// (so two receives of the same name never share one) and always short (a
/// long name never makes it too long for the filesystem).
fn stage_name(id: &str) -> String {
    format!(".desklink-{id}.stage")
}

/// Removes staging files in the downloads folder that nobody has touched since
/// `cutoff`: what a process that died mid-copy leaves, up to a whole file's
/// size. Only regular files named exactly `.desklink-<id>.stage`.
fn sweep_stages(root: &Path, cutoff: SystemTime) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let is_stage = name
            .to_str()
            .is_some_and(|name| name.starts_with(".desklink-") && name.ends_with(".stage"));
        if !is_stage {
            continue;
        }
        let stale = fs::symlink_metadata(entry.path())
            .ok()
            .filter(|metadata| metadata.is_file())
            .and_then(|metadata| metadata.modified().ok())
            .is_some_and(|modified| modified < cutoff);
        if stale {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// Puts the finished `part` at `target` without ever replacing anything: a
/// hard link where the filesystem allows one. Where it does not (the
/// downloads folder is on another filesystem, or one without links such as
/// FAT), the bytes are copied to a hidden file of this transfer beside the
/// target, synced, and linked or renamed into place, so the file still
/// appears whole.
///
/// `AlreadyExists` is returned only when the *target* is taken. The staging
/// file is this transfer's own and is removed only if this call made it.
fn place_file(
    part: &Path,
    target: &Path,
    root: &Path,
    id: &str,
    link: &dyn Fn(&Path, &Path) -> std::io::Result<()>,
) -> std::io::Result<()> {
    match link(part, target) {
        Err(error) if cannot_link(&error) => {}
        other => return other,
    }
    let staged = checked_child(root, &stage_name(id)).map_err(std::io::Error::other)?;
    // A leftover of an earlier try of this very transfer (it died mid-copy):
    // nobody else uses this name.
    if fs::symlink_metadata(&staged).is_ok_and(|metadata| metadata.is_file()) {
        let _ = fs::remove_file(&staged);
    }
    let mut copy = match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&staged)
    {
        Ok(copy) => copy,
        // Not the target: never read as "taken", never removed.
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {
            return Err(std::io::Error::other(
                "something else is in the way of the staging file",
            ));
        }
        Err(error) => return Err(error),
    };
    // From here the staging file is this call's own.
    let copied = (|| {
        let mut source = File::open(part)?;
        std::io::copy(&mut source, &mut copy)?;
        copy.sync_all()
    })();
    drop(copy);
    let placed = copied.and_then(|()| match fs::hard_link(&staged, target) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::AlreadyExists => Err(error),
        // No hard links here either: a rename, once nothing is in the way.
        // (A dangling link at the target is something in the way.)
        Err(_) if fs::symlink_metadata(target).is_err() => fs::rename(&staged, target),
        Err(error) => Err(error),
    });
    let _ = fs::remove_file(&staged);
    placed
}

/// Why a hard link may be refused for a reason a copy gets round.
fn cannot_link(error: &std::io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(libc::EXDEV) | Some(libc::EPERM) | Some(libc::ENOTSUP) | Some(libc::EMLINK)
    ) || error.kind() == ErrorKind::CrossesDevices
}

/// What went wrong, for a person: no room, not allowed, or the system's words.
pub(super) fn plain_io(action: &str, error: &std::io::Error) -> String {
    match error.raw_os_error() {
        Some(libc::ENOSPC) | Some(libc::EDQUOT) => {
            format!("There is no room to {action}. Free some space and try again.")
        }
        Some(libc::EACCES) | Some(libc::EPERM) | Some(libc::EROFS) => {
            format!("DeskLink is not allowed to {action} in the downloads folder.")
        }
        _ => format!("Could not {action}: {error}"),
    }
}

/// The longest prefix of `value` that is at most `max` bytes and ends on a
/// character boundary.
fn truncate_to_boundary(value: &str, max: usize) -> &str {
    let mut end = max.min(value.len());
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

pub(super) fn checked_child(root: &Path, name: &str) -> Result<PathBuf, String> {
    // `.` and `..` are single path components that still resolve outside a
    // plain child, so `components().count()` alone does not exclude them.
    if name.is_empty()
        || matches!(name, "." | "..")
        || Path::new(name).components().count() != 1
        || name.contains(['/', '\\', '\0'])
    {
        return Err("Unsafe DeskLink transfer path".to_string());
    }
    let path = root.join(name);
    if path.parent() != Some(root) {
        return Err("DeskLink transfer path escapes its root".to_string());
    }
    Ok(path)
}

/// Unlinks `path` if something is there. `remove_file` removes a symlink
/// itself and never follows it, so this cannot touch a link's target.
pub(super) fn remove_if_present(path: &Path) -> Result<(), String> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("Could not remove DeskLink transfer file: {error}")),
    }
}

/// Creates an empty file at `path`, replacing whatever entry was there. The
/// old entry is unlinked and the new file created with `O_EXCL`, which
/// refuses to open through a symlink, so a planted link is never written
/// through.
pub(super) fn create_fresh_file(path: &Path) -> Result<File, String> {
    remove_if_present(path)?;
    OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| format!("Could not create DeskLink transfer file: {error}"))
}

pub(super) fn ensure_directory_not_symlink(path: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("Could not inspect DeskLink transfer directory: {error}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err("DeskLink transfer directory is not a safe directory".to_string());
    }
    Ok(())
}

pub(super) fn ensure_regular_file(path: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("Could not inspect DeskLink transfer file: {error}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err("DeskLink transfer path is not a regular file".to_string());
    }
    Ok(())
}

pub(super) fn sync_directory(path: &Path) -> Result<(), String> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| format!("Could not persist DeskLink transfer directory: {error}"))
}

/// A file's modification time in ms since the epoch, as sent in an offer.
pub(super) fn modified_ms(metadata: &fs::Metadata) -> i64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map_or(0, |since| since.as_millis() as i64)
}

/// Whether `error` means the disk (or the user's quota) is full.
pub(super) fn is_storage_full(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        ErrorKind::StorageFull | ErrorKind::QuotaExceeded
    ) || matches!(error.raw_os_error(), Some(libc::ENOSPC | libc::EDQUOT))
}

/// What to show for a peer's error. It is the other device's text, so it is
/// bounded and stripped of control characters before it reaches a window.
/// One that reads like a stack trace (an exception or a class name: a phone
/// once sent "org.desklink.mobile.transport.TransportError: WebRTC data
/// channel is not open") is replaced by plain words.
pub(super) fn peer_reason(error: Option<&str>) -> String {
    let text: String = error
        .unwrap_or_default()
        .chars()
        .filter(|character| !character.is_control())
        .take(256)
        .collect();
    if text.trim().is_empty() {
        return "The other device reported an error".to_string();
    }
    if !looks_technical(&text) {
        return text;
    }
    let lower = text.to_lowercase();
    if [
        "not open",
        "closed",
        "connection",
        "disconnect",
        "socket",
        "timeout",
        "timed out",
    ]
    .iter()
    .any(|hint| lower.contains(hint))
    {
        "The connection dropped".to_string()
    } else {
        "The phone stopped the transfer".to_string()
    }
}

/// Whether `text` names an exception or a class rather than saying what
/// happened.
fn looks_technical(text: &str) -> bool {
    if [
        "Exception",
        "org.",
        "java.",
        "javax.",
        "kotlin.",
        "kotlinx.",
        "android.",
    ]
    .iter()
    .any(|marker| text.contains(marker))
    {
        return true;
    }
    // "some.package.ClassError: …"
    text.split_whitespace().any(|word| {
        let word = word.trim_end_matches(':');
        word.matches('.').count() >= 2 && (word.ends_with("Error") || word.ends_with("Exception"))
    })
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    fn root(label: &str) -> PathBuf {
        let value = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("desklink-store-{label}-{value}"))
    }

    fn store(label: &str) -> (Store, PathBuf) {
        let base = root(label);
        (
            Store::new(base.join("state"), base.join("downloads")).unwrap(),
            base,
        )
    }

    const ID: &str = "0f8fad5b-d9cb-469f-a165-70867728950e";

    fn checkpoint(blocks_done: u64) -> ReceiveCheckpoint {
        ReceiveCheckpoint {
            transfer_id: ID.to_string(),
            device_id: "phone".to_string(),
            token: "token".to_string(),
            filename: "big.bin".to_string(),
            destination_name: "big.bin".to_string(),
            total_size: 5 * BLOCK_BYTES,
            mtime: 1,
            blocks_done,
            clipboard: false,
            capture_id: None,
            folder: None,
            content_sha256: None,
        }
    }

    #[test]
    fn checked_child_only_ever_returns_a_direct_child() {
        let base = Path::new("/tmp/root");
        assert!(checked_child(base, "file.txt").is_ok());
        for bad in ["", ".", "..", "a/b", "../x", "/etc/passwd", "a\\b", "a\0b"] {
            assert!(checked_child(base, bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_checkpoint_round_trips_with_its_hashes() {
        let (store, base) = store("roundtrip");
        let hashes: Vec<[u8; 32]> = (0..3u8).map(|n| [n; 32]).collect();
        let part = store.create_part(ID).unwrap();
        part.set_len(3 * BLOCK_BYTES).unwrap();
        store.save_receive(&checkpoint(3), &hashes).unwrap();
        let (loaded, loaded_hashes) = store.load_receive(ID).unwrap().unwrap();
        assert_eq!(loaded, checkpoint(3));
        assert_eq!(loaded_hashes, hashes);
        store.remove_receive(ID).unwrap();
        assert!(store.load_receive(ID).unwrap().is_none());
        assert!(!store.part_path(ID).unwrap().exists());
        let _ = fs::remove_dir_all(base);
    }

    /// The checkpoint never claims more than the sidecar and the partial file
    /// prove: it is lowered on load.
    #[test]
    fn a_checkpoint_is_lowered_to_what_the_files_support() {
        let (store, base) = store("lowered");
        let hashes: Vec<[u8; 32]> = (0..4u8).map(|n| [n; 32]).collect();
        // The partial file holds only two blocks, the sidecar four.
        let part = store.create_part(ID).unwrap();
        part.set_len(2 * BLOCK_BYTES).unwrap();
        store.save_receive(&checkpoint(4), &hashes).unwrap();
        let (loaded, loaded_hashes) = store.load_receive(ID).unwrap().unwrap();
        assert_eq!(loaded.blocks_done, 2);
        assert_eq!(loaded_hashes.len(), 2);
        // A sidecar shorter than the checkpoint says lowers it too.
        let part = store.create_part(ID).unwrap();
        part.set_len(4 * BLOCK_BYTES).unwrap();
        store.save_receive(&checkpoint(4), &hashes[..1]).unwrap();
        assert_eq!(store.load_receive(ID).unwrap().unwrap().0.blocks_done, 1);
        // No partial file at all: nothing is held.
        fs::remove_file(store.part_path(ID).unwrap()).unwrap();
        assert_eq!(store.load_receive(ID).unwrap().unwrap().0.blocks_done, 0);
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn a_corrupt_or_senseless_checkpoint_is_treated_as_none() {
        let (store, base) = store("corrupt");
        let path = store.state_path(&Store::checkpoint_name(ID)).unwrap();
        for junk in [
            &b"not json"[..],
            b"{}",
            br#"{"transfer_id":"someone-else"}"#,
        ] {
            fs::write(&path, junk).unwrap();
            fs::write(store.part_path(ID).unwrap(), b"leftover").unwrap();
            assert!(store.load_receive(ID).unwrap().is_none());
            assert!(!path.exists(), "removed");
            assert!(!store.part_path(ID).unwrap().exists(), "and its partial");
        }
        // Parses, makes no sense: more blocks than the file has.
        let mut silly = checkpoint(99);
        silly.total_size = BLOCK_BYTES;
        fs::write(&path, serde_json::to_vec(&silly).unwrap()).unwrap();
        assert!(store.load_receive(ID).unwrap().is_none());
        // An unsafe name.
        let mut unsafe_name = checkpoint(0);
        unsafe_name.destination_name = "../escape".to_string();
        fs::write(&path, serde_json::to_vec(&unsafe_name).unwrap()).unwrap();
        assert!(store.load_receive(ID).unwrap().is_none());
        // A symlink where the checkpoint belongs is not a checkpoint.
        std::os::unix::fs::symlink("/etc/hostname", &path).unwrap();
        assert!(store.load_receive(ID).unwrap().is_none());
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn a_send_checkpoint_round_trips_and_a_tampered_one_is_dropped() {
        let (store, base) = store("send");
        let send = SendCheckpoint {
            transfer_id: ID.to_string(),
            device_id: "phone".to_string(),
            token: "token".to_string(),
            filename: "a.txt".to_string(),
            total_size: 3,
            mtime: 5,
            source: PathBuf::from("/home/me/a.txt"),
            history: true,
            clipboard_mime: None,
            folder: None,
            content_sha256: None,
            restarts: 1,
        };
        store.save_send(&send).unwrap();
        assert_eq!(store.stored_sends(), std::slice::from_ref(&send));
        // A relative source, or a name that is not one, is not trusted.
        let mut tampered = send.clone();
        tampered.source = PathBuf::from("relative");
        store.save_send(&tampered).unwrap();
        assert!(store.stored_sends().is_empty());
        assert!(!store.state_path(&Store::send_name(ID)).unwrap().exists());
        store.save_send(&send).unwrap();
        store.remove_send(ID).unwrap();
        assert!(store.stored_sends().is_empty());
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn downloads_never_overwrite_and_collisions_become_name_2_name_3() {
        let (store, base) = store("collisions");
        let downloads = base.join("downloads");
        fs::write(downloads.join("report.pdf"), b"first").unwrap();
        assert_eq!(
            store.available_destination_name("report.pdf").unwrap(),
            "report (2).pdf"
        );
        fs::write(downloads.join("report (2).pdf"), b"second").unwrap();
        assert_eq!(
            store.available_destination_name("report.pdf").unwrap(),
            "report (3).pdf"
        );
        assert_eq!(
            store.available_destination_name("fresh.pdf").unwrap(),
            "fresh.pdf"
        );
        // No extension, and a dotfile.
        fs::write(downloads.join("README"), b"x").unwrap();
        assert_eq!(
            store.available_destination_name("README").unwrap(),
            "README (2)"
        );
        // A dangling symlink occupies a name too.
        std::os::unix::fs::symlink("/nonexistent", downloads.join("dangling.txt")).unwrap();
        assert_eq!(
            store.available_destination_name("dangling.txt").unwrap(),
            "dangling (2).txt"
        );
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn a_collision_suffix_never_pushes_the_name_past_the_filesystem_limit() {
        let (store, base) = store("long-collision");
        let stem = "x".repeat(251);
        let name = format!("{stem}.txt");
        assert_eq!(name.len(), 255);
        fs::write(base.join("downloads").join(&name), b"x").unwrap();
        let next = store.available_destination_name(&name).unwrap();
        assert!(next.len() <= 255 && next.ends_with(" (2).txt"), "{next}");
        // Multi-byte characters are cut on a character boundary.
        let wide = format!("{}.bin", "é".repeat(125));
        fs::write(base.join("downloads").join(&wide), b"x").unwrap();
        let next = store.available_destination_name(&wide).unwrap();
        assert!(next.len() <= 255 && next.ends_with(" (2).bin"));
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn a_symlink_at_the_destination_is_never_followed_or_replaced() {
        let (store, base) = store("symlink-destination");
        let victim = base.join("victim.txt");
        fs::write(&victim, b"precious").unwrap();
        std::os::unix::fs::symlink(&victim, base.join("downloads").join("a.txt")).unwrap();
        fs::write(store.part_path(ID).unwrap(), b"new").unwrap();
        let name = store.available_destination_name("a.txt").unwrap();
        assert_eq!(name, "a (2).txt");
        let published = store.publish(ID, &name, "a.txt").unwrap();
        assert_eq!(fs::read(published).unwrap(), b"new");
        assert_eq!(fs::read(&victim).unwrap(), b"precious", "untouched");
        // A name taken between choosing and publishing moves on.
        fs::write(store.part_path(ID).unwrap(), b"newer").unwrap();
        let again = store.publish(ID, "a (2).txt", "a.txt").unwrap();
        assert!(again.ends_with("a (3).txt"), "{again:?}");
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn a_symlink_planted_at_a_temporary_path_is_never_written_through() {
        let (store, base) = store("symlink-tmp");
        let victim = base.join("victim.txt");
        fs::write(&victim, b"precious").unwrap();
        let tmp = store
            .state_path(&format!("{}.tmp", Store::checkpoint_name(ID)))
            .unwrap();
        std::os::unix::fs::symlink(&victim, &tmp).unwrap();
        store.save_receive(&checkpoint(0), &[]).unwrap();
        assert_eq!(fs::read(&victim).unwrap(), b"precious");
        assert!(store.load_receive(ID).unwrap().is_some());
        // The same for a swapped-in partial file.
        let part = store.part_path(ID).unwrap();
        fs::remove_file(&part).ok();
        std::os::unix::fs::symlink(&victim, &part).unwrap();
        assert!(store.open_part(ID).is_err());
        assert_eq!(fs::read(&victim).unwrap(), b"precious");
        let fresh = store.create_part(ID).unwrap();
        drop(fresh);
        assert_eq!(fs::read(&victim).unwrap(), b"precious");
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn a_symlinked_state_directory_is_refused_and_a_symlinked_downloads_one_is_resolved() {
        let base = root("symlink-dirs");
        let real = base.join("real");
        fs::create_dir_all(&real).unwrap();
        std::os::unix::fs::symlink(&real, base.join("link")).unwrap();
        assert!(Store::new(base.join("link"), base.join("downloads")).is_err());
        // `~/Downloads` as a link works: the real directory is used (and was
        // refused before, which stopped DeskLink receiving anything).
        let store = Store::new(base.join("state"), base.join("link")).unwrap();
        assert_eq!(
            store.destination_path("a.bin").unwrap(),
            fs::canonicalize(&real).unwrap().join("a.bin")
        );
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn a_transfer_nobody_touched_for_a_week_is_swept_but_recent_ones_stay() {
        let (store, base) = store("sweep");
        let state = base.join("state");
        let id = "11111111-1111-4111-8111-111111111111";
        for suffix in [".part", ".json", ".blocks", ".send.json", ".sync", ".tmp"] {
            fs::write(state.join(format!(".desklink-{id}{suffix}")), b"x").unwrap();
        }
        fs::write(state.join("unrelated.txt"), b"keep").unwrap();
        fs::write(state.join(".desklink-recent.part"), b"keep").unwrap();
        // Nothing is old yet.
        store.sweep_stale(Duration::from_secs(60));
        assert_eq!(fs::read_dir(&state).unwrap().count(), 8);
        // Everything older than "now" goes; unrelated files never do.
        std::thread::sleep(Duration::from_millis(20));
        store.sweep_stale(Duration::ZERO);
        let left: Vec<_> = fs::read_dir(&state)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(left, ["unrelated.txt"]);
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn a_peers_exception_is_never_shown_as_the_reason() {
        assert_eq!(
            peer_reason(Some("Not enough space")),
            "Not enough space",
            "plain words stay"
        );
        assert_eq!(peer_reason(None), "The other device reported an error");
        assert_eq!(
            peer_reason(Some(
                "org.desklink.mobile.transport.TransportError: WebRTC data channel is not open"
            )),
            "The connection dropped"
        );
        assert_eq!(
            peer_reason(Some("java.lang.IllegalStateException: boom")),
            "The phone stopped the transfer"
        );
        assert_eq!(peer_reason(Some(&"x".repeat(1000))).chars().count(), 256);
        assert_eq!(peer_reason(Some("a\u{7}b")), "ab");
    }

    #[test]
    fn a_full_disk_is_recognised() {
        assert!(is_storage_full(&std::io::Error::from_raw_os_error(
            libc::ENOSPC
        )));
        assert!(is_storage_full(&std::io::Error::from(
            ErrorKind::StorageFull
        )));
        assert!(!is_storage_full(&std::io::Error::from(
            ErrorKind::PermissionDenied
        )));
    }

    #[test]
    fn free_space_can_be_read() {
        let (store, base) = store("free");
        assert!(store.free_bytes().is_some_and(|bytes| bytes > 0));
        let _ = fs::remove_dir_all(base);
    }
}
