//! Files received from phones and sent to them, for the Transfers page.
//!
//! One JSON object per line in `$XDG_STATE_HOME/desklink/transfers.jsonl`,
//! kept like the connection history (same retention, same file helpers):
//! appended when a file lands or has been sent, pruned at startup.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::connection_log::{
    append_line, cached, name_of, now, prune_lines, read_lines, rewrite_lines, LogCache,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoggedFile {
    /// Unix seconds.
    pub ts: u64,
    pub device_id: String,
    pub device_name: String,
    /// The file's name as sent.
    pub name: String,
    /// Where it is on this computer: saved here, or sent from here.
    pub path: PathBuf,
    pub size: u64,
    /// Received from the phone; false for a file sent to it. Entries
    /// written before sent files were logged are all received ones.
    #[serde(default = "received")]
    pub incoming: bool,
}

fn received() -> bool {
    true
}

fn log_path() -> Option<PathBuf> {
    // Unit tests run the real transfer paths: they must never write the
    // person's history.
    if cfg!(test) {
        return Some(std::env::temp_dir().join(format!(
            "desklink-test-transfers-{}.jsonl",
            std::process::id()
        )));
    }
    Some(
        dirs::state_dir()
            .or_else(dirs::data_local_dir)?
            .join("desklink")
            .join("transfers.jsonl"),
    )
}

/// Records a file received from `device_id`, now at `path`.
pub fn record_received(device_id: &str, name: &str, path: &std::path::Path) {
    let size = std::fs::metadata(path).map_or(0, |meta| meta.len());
    record(device_id, name, path, size, true);
}

/// Records a file at `path` sent to `device_id`: `size` bytes as sent (the
/// file may have changed since).
pub fn record_sent(device_id: &str, name: &str, path: &std::path::Path, size: u64) {
    record(device_id, name, path, size, false);
}

fn record(device_id: &str, name: &str, path: &std::path::Path, size: u64, incoming: bool) {
    let entry = LoggedFile {
        ts: now(),
        device_id: device_id.to_string(),
        device_name: name_of(device_id),
        name: name.to_string(),
        path: path.to_path_buf(),
        size,
        incoming,
    };
    if let Some(log) = log_path() {
        append_line(&log, &entry);
    }
}

/// Takes `entry` off the list ("Remove from list"); the file stays.
pub fn remove(entry: &LoggedFile) {
    if let Some(log) = log_path() {
        rewrite_lines(
            &log,
            now(),
            |file: &LoggedFile| file.ts,
            |file| file != entry,
        );
    }
}

/// Every file received in the retention period, newest first.
pub fn recent() -> Vec<LoggedFile> {
    static CACHE: LogCache<LoggedFile> = std::sync::Mutex::new(None);
    let now = now();
    log_path()
        .map(|log| {
            cached(&CACHE, &log, now, || {
                read_lines(&log, now, |entry: &LoggedFile| entry.ts)
            })
        })
        .unwrap_or_default()
}

/// Drops entries past the retention period. Called once at startup.
pub fn prune() {
    if let Some(log) = log_path() {
        prune_lines(&log, now(), |entry: &LoggedFile| entry.ts);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device_links::connection_log::RETENTION;

    fn file(ts: u64, name: &str) -> LoggedFile {
        LoggedFile {
            ts,
            device_id: "phone".into(),
            device_name: "Pixel".into(),
            name: name.into(),
            path: PathBuf::from("/tmp").join(name),
            size: 12,
            incoming: true,
        }
    }

    #[test]
    fn received_files_older_than_thirty_days_are_pruned() {
        let dir = std::env::temp_dir().join(format!("desklink-xfer-{}", uuid::Uuid::new_v4()));
        let path = dir.join("transfers.jsonl");
        let now = 100 * 24 * 60 * 60;
        append_line(&path, &file(now - RETENTION.as_secs() - 1, "old.jpg"));
        append_line(&path, &file(now - 10, "a.pdf"));
        append_line(&path, &file(now - 5, "b.png"));

        let read = |path: &PathBuf| read_lines(path, now, |entry: &LoggedFile| entry.ts);
        let names: Vec<_> = read(&path).into_iter().map(|entry| entry.name).collect();
        assert_eq!(names, ["b.png", "a.pdf"], "newest first, old one hidden");

        prune_lines(&path, now, |entry: &LoggedFile| entry.ts);
        assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 2);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The window asks every second; the file is parsed again only once it
    /// has changed.
    #[test]
    fn a_log_is_read_again_only_when_it_changes() {
        let dir = std::env::temp_dir().join(format!("desklink-cache-{}", uuid::Uuid::new_v4()));
        let path = dir.join("transfers.jsonl");
        let cache: LogCache<LoggedFile> = std::sync::Mutex::new(None);
        let reads = std::cell::Cell::new(0);
        let now = 100 * 24 * 60 * 60;
        let read = || {
            reads.set(reads.get() + 1);
            read_lines(&path, now, |entry: &LoggedFile| entry.ts)
        };
        append_line(&path, &file(now - 5, "a.pdf"));
        assert_eq!(cached(&cache, &path, now, read).len(), 1);
        assert_eq!(cached(&cache, &path, now, read).len(), 1);
        assert_eq!(reads.get(), 1, "unchanged file served from the cache");
        append_line(&path, &file(now - 1, "b.png"));
        assert_eq!(cached(&cache, &path, now, read).len(), 2);
        assert_eq!(reads.get(), 2);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn older_entries_read_as_received_and_one_can_be_removed() {
        let old: LoggedFile = serde_json::from_str(
            r#"{"ts":1,"device_id":"p","device_name":"P","name":"a.jpg","path":"/tmp/a.jpg","size":3}"#,
        )
        .unwrap();
        assert!(old.incoming);

        let dir = std::env::temp_dir().join(format!("desklink-remove-{}", uuid::Uuid::new_v4()));
        let path = dir.join("transfers.jsonl");
        let now = 100 * 24 * 60 * 60;
        let (a, mut b) = (file(now - 9, "a.pdf"), file(now - 8, "b.png"));
        b.incoming = false;
        append_line(&path, &a);
        append_line(&path, &b);
        rewrite_lines(&path, now, |e: &LoggedFile| e.ts, |e| *e != a);
        let left: Vec<LoggedFile> = read_lines(&path, now, |e: &LoggedFile| e.ts);
        assert_eq!(left, [b]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn tests_never_write_the_real_log() {
        let path = log_path().unwrap();
        assert!(path.starts_with(std::env::temp_dir()), "{}", path.display());
    }
}
