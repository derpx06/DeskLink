//! Connection history: who reached this computer, when, and what they did.
//!
//! One JSON object per line in `$XDG_STATE_HOME/desklink/connections.jsonl`,
//! appended from whichever thread saw the event and pruned to the last
//! [`RETENTION`] at startup. Lines that fail to parse are skipped, never
//! fatal: a history view must not stop DeskLink starting.

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

pub const RETENTION: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// What happened. Serialized in kebab-case, so the file reads plainly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ConnectionEvent {
    Connected,
    Disconnected,
    ScreenViewStarted,
    ScreenControlStarted,
    ScreenSessionEnded,
    TerminalAttached,
    TerminalDetached,
    PasswordRejected,
    SessionsEnded,
    /// The connection's path was learned or changed (see `path`).
    Route,
    /// The phone changed one of this computer's settings (`detail`:
    /// "key=value").
    SettingsChanged,
    /// The phone asked to unlock this computer and was refused (`detail`:
    /// the reason code).
    UnlockRefused,
    UnlockedByPhone,
}

impl ConnectionEvent {
    pub fn label(self) -> &'static str {
        match self {
            Self::Connected => "Connected",
            Self::Disconnected => "Disconnected",
            Self::ScreenViewStarted => "Started viewing the screen",
            Self::ScreenControlStarted => "Started controlling the screen",
            Self::ScreenSessionEnded => "Stopped viewing the screen",
            Self::TerminalAttached => "Opened a terminal",
            Self::TerminalDetached => "Left a terminal",
            Self::PasswordRejected => "Wrong session password",
            Self::SessionsEnded => "Sessions ended from this computer",
            Self::Route => "Connection path",
            Self::SettingsChanged => "Changed a setting here",
            Self::UnlockRefused => "Unlock refused",
            Self::UnlockedByPhone => "Unlocked this computer",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionEntry {
    /// Unix seconds.
    pub ts: u64,
    pub device_id: String,
    pub device_name: String,
    pub event: ConnectionEvent,
    /// How the devices were connected (`"lan"`, `"direct"`, `"turn"`), when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// What changed or why, for the events that say (`settings-changed`,
    /// `unlock-refused`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Serializes appends and the startup prune, so lines never interleave.
static FILE_LOCK: Mutex<()> = Mutex::new(());

fn log_path() -> Option<PathBuf> {
    // Unit tests run the real daemon paths: they must never write the
    // person's history.
    if cfg!(test) {
        return Some(std::env::temp_dir().join(format!(
            "desklink-test-connections-{}.jsonl",
            std::process::id()
        )));
    }
    Some(
        dirs::state_dir()
            .or_else(dirs::data_local_dir)?
            .join("desklink")
            .join("connections.jsonl"),
    )
}

pub(crate) fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

/// Names of devices seen this run, so callers that only know a device id
/// (the peer workers) still write a readable entry.
static NAMES: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);

pub fn remember_name(device_id: &str, device_name: &str) {
    if let Ok(mut names) = NAMES.lock() {
        let names = names.get_or_insert_with(HashMap::new);
        if names.get(device_id).map(String::as_str) != Some(device_name) {
            names.insert(device_id.to_string(), device_name.to_string());
        }
    }
}

pub(crate) fn name_of(device_id: &str) -> String {
    NAMES
        .lock()
        .ok()
        .and_then(|names| names.as_ref()?.get(device_id).cloned())
        .unwrap_or_default()
}

/// Appends one entry. Best effort: history is never worth failing a session.
pub fn record(device_id: &str, event: ConnectionEvent) {
    record_with_path(device_id, event, None);
}

pub fn record_with_path(device_id: &str, event: ConnectionEvent, path: Option<&str>) {
    write_entry(device_id, event, path, None);
}

/// The devices last logged as connected (feature-ready).
static READY: Mutex<Option<std::collections::HashSet<String>>> = Mutex::new(None);

/// Whether a readiness change is news: `ready` differs from what was last
/// logged for `device_id`. Updates the record.
fn ready_edge(
    logged: &mut std::collections::HashSet<String>,
    device_id: &str,
    ready: bool,
) -> bool {
    if ready {
        logged.insert(device_id.to_string())
    } else {
        logged.remove(device_id)
    }
}

/// Logs `Connected`/`Disconnected` on a feature-ready edge. Called where the
/// transition happens — the handover reaching feature-ready, a peer being
/// cleared or replaced — so a sub-second rebuild is logged too; the 1 Hz
/// reconcile only catches what those missed.
pub fn note_ready(device_id: &str, ready: bool) {
    let news = READY.lock().is_ok_and(|mut logged| {
        ready_edge(
            logged.get_or_insert_with(Default::default),
            device_id,
            ready,
        )
    });
    if news && !ready {
        crate::device_links::automations::phone_link(device_id, false);
        crate::device_links::folders::link(device_id, false);
    }
    if news {
        record(
            device_id,
            if ready {
                ConnectionEvent::Connected
            } else {
                ConnectionEvent::Disconnected
            },
        );
    }
}

/// Records `event` with what changed or why.
pub fn record_detail(device_id: &str, event: ConnectionEvent, detail: &str) {
    write_entry(device_id, event, None, Some(detail));
}

fn write_entry(device_id: &str, event: ConnectionEvent, path: Option<&str>, detail: Option<&str>) {
    let entry = ConnectionEntry {
        ts: now(),
        device_id: device_id.to_string(),
        device_name: name_of(device_id),
        event,
        path: path.map(str::to_string),
        detail: detail.map(str::to_string),
    };
    if let Some(path) = log_path() {
        append(&path, &entry);
    }
}

fn append(path: &PathBuf, entry: &ConnectionEntry) {
    append_line(path, entry);
}

/// Appends `entry` as one JSON line; shared with the received-files log.
pub(crate) fn append_line(path: &PathBuf, entry: &impl Serialize) {
    let Ok(mut line) = serde_json::to_vec(entry) else {
        return;
    };
    line.push(b'\n');
    let _guard = FILE_LOCK.lock();
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = file.write_all(&line);
    }
}

/// Every entry from the last [`RETENTION`], newest first.
pub fn recent() -> Vec<ConnectionEntry> {
    static CACHE: LogCache<ConnectionEntry> = Mutex::new(None);
    let now = now();
    log_path()
        .map(|path| cached(&CACHE, &path, now, || read(&path, now)))
        .unwrap_or_default()
}

fn read(path: &PathBuf, now: u64) -> Vec<ConnectionEntry> {
    read_lines(path, now, |entry: &ConnectionEntry| entry.ts)
}

/// The entries of a JSON-lines log from the last [`RETENTION`], newest
/// first; `ts` gives each one's Unix seconds. Unreadable lines are skipped.
pub(crate) fn read_lines<T: serde::de::DeserializeOwned>(
    path: &PathBuf,
    now: u64,
    ts: impl Fn(&T) -> u64,
) -> Vec<T> {
    let _guard = FILE_LOCK.lock();
    read_unlocked(path, now, ts)
}

/// [`read_lines`] for a caller already holding `FILE_LOCK`.
fn read_unlocked<T: serde::de::DeserializeOwned>(
    path: &PathBuf,
    now: u64,
    ts: impl Fn(&T) -> u64,
) -> Vec<T> {
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let oldest = now.saturating_sub(RETENTION.as_secs());
    let mut entries: Vec<T> = text
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .filter(|entry| ts(entry) >= oldest)
        .collect();
    entries.reverse();
    entries
}

/// When a log file last changed, as far as a reader can tell: its
/// modification time and length, and the day (so entries still age out).
type Stamp = (Option<SystemTime>, u64, u64);

/// A log's entries as last read, and the file's [`Stamp`] then.
pub(crate) type LogCache<T> = Mutex<Option<(Stamp, Vec<T>)>>;

/// The log at `path`, re-read only when the file has changed since the
/// cached copy: the window asks once a second, and a month of history is
/// not worth parsing that often.
pub(crate) fn cached<T: Clone>(
    cache: &LogCache<T>,
    path: &PathBuf,
    now: u64,
    read: impl FnOnce() -> Vec<T>,
) -> Vec<T> {
    let meta = fs::metadata(path).ok();
    let stamp = (
        meta.as_ref().and_then(|meta| meta.modified().ok()),
        meta.map_or(0, |meta| meta.len()),
        now / 86_400,
    );
    let Ok(mut cache) = cache.lock() else {
        return read();
    };
    if let Some((cached, entries)) = cache.as_ref() {
        if *cached == stamp {
            return entries.clone();
        }
    }
    let entries = read();
    *cache = Some((stamp, entries.clone()));
    entries
}

/// Drops entries older than [`RETENTION`]. Called once at startup.
pub fn prune() {
    if let Some(path) = log_path() {
        prune_file(&path, now());
    }
}

fn prune_file(path: &PathBuf, now: u64) {
    prune_lines(path, now, |entry: &ConnectionEntry| entry.ts);
}

/// Rewrites a JSON-lines log without entries older than [`RETENTION`].
/// The lock is held from the read to the rename, so an entry appended
/// meanwhile cannot be lost.
pub(crate) fn prune_lines<T: Serialize + serde::de::DeserializeOwned>(
    path: &PathBuf,
    now: u64,
    ts: impl Fn(&T) -> u64,
) {
    rewrite_lines(path, now, ts, |_: &T| true);
}

/// Rewrites a JSON-lines log with only the recent entries `keep` accepts,
/// under the lock from read to rename.
pub(crate) fn rewrite_lines<T: Serialize + serde::de::DeserializeOwned>(
    path: &PathBuf,
    now: u64,
    ts: impl Fn(&T) -> u64,
    keep: impl Fn(&T) -> bool,
) {
    let _guard = FILE_LOCK.lock();
    let mut kept: Vec<T> = read_unlocked(path, now, ts);
    kept.retain(|entry| keep(entry));
    kept.reverse();
    let mut text = String::new();
    for entry in kept {
        if let Ok(line) = serde_json::to_string(&entry) {
            text.push_str(&line);
            text.push('\n');
        }
    }
    let temporary = path.with_extension("jsonl.tmp");
    if fs::write(&temporary, text).is_ok() {
        let _ = fs::rename(&temporary, path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(ts: u64, event: ConnectionEvent) -> ConnectionEntry {
        ConnectionEntry {
            ts,
            device_id: "phone".into(),
            device_name: "Pixel".into(),
            event,
            path: None,
            detail: None,
        }
    }

    #[test]
    fn entries_read_back_newest_first_and_old_ones_are_pruned() {
        let dir = std::env::temp_dir().join(format!("desklink-log-{}", uuid::Uuid::new_v4()));
        let path = dir.join("connections.jsonl");
        let now = 100 * 24 * 60 * 60;
        let old = now - RETENTION.as_secs() - 1;
        append(&path, &entry(old, ConnectionEvent::Connected));
        append(&path, &entry(now - 10, ConnectionEvent::Connected));
        append(
            &path,
            &entry(now - 5, ConnectionEvent::ScreenControlStarted),
        );
        fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"not json\n")
            .unwrap();

        let entries = read(&path, now);
        assert_eq!(
            entries.iter().map(|e| e.event).collect::<Vec<_>>(),
            [
                ConnectionEvent::ScreenControlStarted,
                ConnectionEvent::Connected
            ]
        );

        prune_file(&path, now);
        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 2);
        assert!(text.contains("\"screen-control-started\""));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_rebuild_is_logged_once_each_way() {
        let mut logged = std::collections::HashSet::new();
        assert!(ready_edge(&mut logged, "phone", true), "connected");
        assert!(!ready_edge(&mut logged, "phone", true), "no repeat");
        assert!(ready_edge(&mut logged, "phone", false), "peer cleared");
        assert!(
            !ready_edge(&mut logged, "phone", false),
            "reconcile repeats nothing"
        );
        assert!(ready_edge(&mut logged, "phone", true), "rebuilt");
    }

    #[test]
    fn older_entries_without_a_detail_still_read() {
        let old = r#"{"ts":5,"device_id":"phone","device_name":"Pixel","event":"connected"}"#;
        let parsed: ConnectionEntry = serde_json::from_str(old).unwrap();
        assert_eq!(parsed.detail, None);
        let mut refused = entry(6, ConnectionEvent::UnlockRefused);
        refused.detail = Some("not-lan".into());
        let line = serde_json::to_string(&refused).unwrap();
        assert!(line.contains("\"detail\":\"not-lan\""), "{line}");
        assert!(
            !serde_json::to_string(&entry(7, ConnectionEvent::Connected))
                .unwrap()
                .contains("detail")
        );
    }
}
