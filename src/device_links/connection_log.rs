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
}

/// Serializes appends and the startup prune, so lines never interleave.
static FILE_LOCK: Mutex<()> = Mutex::new(());

fn log_path() -> Option<PathBuf> {
    Some(
        dirs::state_dir()
            .or_else(dirs::data_local_dir)?
            .join("desklink")
            .join("connections.jsonl"),
    )
}

fn now() -> u64 {
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

fn name_of(device_id: &str) -> String {
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
    let entry = ConnectionEntry {
        ts: now(),
        device_id: device_id.to_string(),
        device_name: name_of(device_id),
        event,
        path: path.map(str::to_string),
    };
    if let Some(path) = log_path() {
        append(&path, &entry);
    }
}

fn append(path: &PathBuf, entry: &ConnectionEntry) {
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
    log_path()
        .map(|path| read(&path, now()))
        .unwrap_or_default()
}

fn read(path: &PathBuf, now: u64) -> Vec<ConnectionEntry> {
    let _guard = FILE_LOCK.lock();
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let oldest = now.saturating_sub(RETENTION.as_secs());
    let mut entries: Vec<ConnectionEntry> = text
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .filter(|entry: &ConnectionEntry| entry.ts >= oldest)
        .collect();
    entries.reverse();
    entries
}

/// Drops entries older than [`RETENTION`]. Called once at startup.
pub fn prune() {
    if let Some(path) = log_path() {
        prune_file(&path, now());
    }
}

fn prune_file(path: &PathBuf, now: u64) {
    let mut kept = read(path, now);
    kept.reverse();
    let mut text = String::new();
    for entry in kept {
        if let Ok(line) = serde_json::to_string(&entry) {
            text.push_str(&line);
            text.push('\n');
        }
    }
    let _guard = FILE_LOCK.lock();
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
}
