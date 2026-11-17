//! The update thread and the handle the rest of the app holds. The thread
//! owns the [`Engine`]; everyone else reads a [`Snapshot`] and sends
//! [`Command`]s over a channel, the same boundary as the daemon's handle: the
//! UI never waits on the network, and nothing here is ever joined or waited
//! for by the daemon. A panic in a cycle is caught and becomes a failed check
//! (`internal`); the thread carries on.
//!
//! It refreshes the cached answer about once a day while the process runs
//! (no alarm, no wake lock). That refresh only updates the snapshot, and so
//! the top bar's properties; it can never open anything.
//!
//! Sibling edges: `engine`, `fetch`, `keys`, `model`, `schedule`. Outside:
//! `device_links::config::app_settings` (read once per cycle).

use super::engine::{Engine, Settings, Status};
use super::fetch::{Fetch, HttpsFetcher};
use super::model::{Channel, Decision, Version};
use super::schedule;
use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

/// The update address baked in at build time (`DESKLINK_UPDATE_URL`, like the
/// relay's). Empty means the feature is inert and hidden. Release builds have
/// no other source: no config key, no environment variable.
pub(crate) const BUILD_URL: &str = match option_env!("DESKLINK_UPDATE_URL") {
    Some(url) => url,
    None => "",
};

/// The address in use: debug builds may override it with the environment (the
/// local test server); release builds never.
fn configured_url() -> String {
    #[cfg(debug_assertions)]
    if let Ok(url) = std::env::var("DESKLINK_UPDATE_URL") {
        return url.trim().to_string();
    }
    BUILD_URL.trim().to_string()
}

/// Everything the UI and the top bar read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Snapshot {
    /// Disabled and NotConfigured override; else what the last check left.
    pub(crate) status: Status,
    /// The outcome of the last check itself (Idle or Failed), whatever the
    /// settings say, for the Settings row.
    pub(crate) last_check: Status,
    /// The manifest's meaning now, as an automatic open reads it.
    pub(crate) decision: Decision,
    /// The same read as a manual check (ignores rollout, skip, snooze).
    pub(crate) manual_decision: Decision,
    pub(crate) installed: Version,
    pub(crate) channel: Channel,
    /// Unix seconds; 0 never.
    pub(crate) last_checked: u64,
    /// Seconds since the cache was confirmed; `None` with no cache.
    pub(crate) cache_age: Option<u64>,
    /// The version the person skipped, while one is.
    pub(crate) skipped: Option<Version>,
    /// Counts completed checks, so a caller can tell its check finished.
    pub(crate) generation: u64,
}

#[derive(Debug)]
pub(crate) enum Command {
    /// `manual`: the person asked (ignores the setting and the rollout).
    Check {
        manual: bool,
    },
    Skip(Version),
    Unskip,
    Snooze(Version),
    /// A setting changed: read it again.
    Settings,
}

pub(crate) struct UpdateHandle {
    shared: Arc<Mutex<Snapshot>>,
    commands: Sender<Command>,
}

impl UpdateHandle {
    pub(crate) fn snapshot(&self) -> Snapshot {
        self.shared
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub(crate) fn send(&self, command: Command) {
        let _ = self.commands.send(command);
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn settings() -> Settings {
    let app = crate::device_links::config::app_settings();
    Settings {
        auto_check: app.update_check,
        channel: app.update_channel,
    }
}

/// The top bar's three read-only values: true while an update is on offer
/// (`Available`, `Snoozed`, `Required`), false for skipped, up to date, off
/// or not set up; the version to name; whether it is required.
pub(crate) fn top_bar(snapshot: &Snapshot) -> (bool, String, bool) {
    if matches!(snapshot.status, Status::Disabled | Status::NotConfigured) {
        return (false, String::new(), false);
    }
    match &snapshot.decision {
        Decision::Available(c) | Decision::Snoozed(c, _) => (true, c.version.to_string(), false),
        Decision::Required(c) => (true, c.version.to_string(), true),
        Decision::UpToDate | Decision::Skipped(_) => (false, String::new(), false),
    }
}

static HANDLE: OnceLock<UpdateHandle> = OnceLock::new();

/// The running service, once [`start`] has been called.
pub(crate) fn handle() -> Option<&'static UpdateHandle> {
    HANDLE.get()
}

/// Starts the thread once. With no address in the build it still answers
/// (`NotConfigured`) and never touches the network.
pub(crate) fn start() -> &'static UpdateHandle {
    HANDLE.get_or_init(|| {
        let url = configured_url();
        let installed = Version::parse(crate::config::VERSION);
        let fetcher = installed.and_then(|_| HttpsFetcher::new(&url));
        spawn(
            fetcher,
            super::keys::trusted(),
            installed.unwrap_or(Version(0, 0, 0)),
            Some(super::store::Store::default_path()),
        )
    })
}

/// How long the thread sleeps before it next looks: until the next automatic
/// check is due (never under 30 s), or, with automatic checks off, until a
/// command wakes it (a year of sleep): an idle desk is not woken every 30 s.
pub(super) fn wait_secs(auto_check: bool, due: u64, now: u64) -> u64 {
    if auto_check {
        due.saturating_sub(now).max(30)
    } else {
        schedule::IDLE_WAIT
    }
}

fn snapshot_of<F: Fetch>(
    engine: &Engine<F>,
    settings: &Settings,
    generation: u64,
    now: u64,
) -> Snapshot {
    let installed = engine.installed();
    let status = match &engine.status {
        Status::NotConfigured => Status::NotConfigured,
        _ if !settings.auto_check => Status::Disabled,
        other => other.clone(),
    };
    Snapshot {
        status,
        last_check: engine.status.clone(),
        decision: engine.decision(settings, now, false),
        manual_decision: engine.decision(settings, now, true),
        installed,
        channel: Channel::chosen(&settings.channel, installed),
        last_checked: engine.store.last_checked,
        cache_age: engine.cache_age(now),
        skipped: engine.prefs().skipped,
        generation,
    }
}

pub(super) fn spawn<F: Fetch + 'static>(
    fetcher: Option<F>,
    keys: Vec<super::model::Key>,
    installed: Version,
    path: Option<std::path::PathBuf>,
) -> UpdateHandle {
    let (commands, inbox) = channel::<Command>();
    if fetcher.is_none() {
        // No address in this build: no thread, no state file, nothing read.
        let inert = Snapshot {
            status: Status::NotConfigured,
            last_check: Status::NotConfigured,
            decision: Decision::UpToDate,
            manual_decision: Decision::UpToDate,
            installed,
            channel: Channel::chosen("", installed),
            last_checked: 0,
            cache_age: None,
            skipped: None,
            generation: 0,
        };
        return UpdateHandle {
            shared: Arc::new(Mutex::new(inert)),
            commands,
        };
    }
    let mut engine = Engine::new(fetcher, keys, installed, path, now());
    let first = snapshot_of(&engine, &settings(), 0, now());
    let shared = Arc::new(Mutex::new(first));
    let published = shared.clone();
    let _ = std::thread::Builder::new()
        .name("desklink-update".into())
        .spawn(move || {
            let mut generation = 0u64;
            let mut seed = u64::from(now() as u32) ^ 0x9e37_79b9;
            loop {
                let wait = wait_secs(settings().auto_check, engine.next_due(seed), now());
                let command = match inbox.recv_timeout(Duration::from_secs(wait)) {
                    Ok(command) => Some(command),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => return,
                };
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let current = settings();
                    let at = now();
                    let checked = match command {
                        None => {
                            engine.check(&current, at, false);
                            true
                        }
                        Some(Command::Check { manual }) => {
                            engine.check(&current, at, manual);
                            true
                        }
                        Some(Command::Skip(v)) => {
                            engine.skip(v);
                            false
                        }
                        Some(Command::Unskip) => {
                            engine.unskip();
                            false
                        }
                        Some(Command::Snooze(v)) => {
                            engine.snooze(v, at);
                            false
                        }
                        Some(Command::Settings) => false,
                    };
                    (checked, current)
                }));
                let (checked, current) = match outcome {
                    Ok(done) => done,
                    Err(_) => {
                        debug_log!("[Update] a check panicked; carrying on");
                        (false, settings())
                    }
                };
                if checked {
                    generation += 1;
                }
                seed = seed
                    .rotate_left(13)
                    .wrapping_mul(0x2545_f491_4f6c_dd1d)
                    .wrapping_add(1);
                if let Ok(mut slot) = published.lock() {
                    *slot = snapshot_of(&engine, &current, generation, now());
                }
            }
        });
    UpdateHandle { shared, commands }
}
