//! How far the Bucket's files have got (F8b): per file and per phone, with a
//! smoothed speed and the time left, for the Bucket page, the overview card,
//! the top-bar extension, the tray menu, and the "up to date" notification.
//!
//! Read from the transfer board's Bucket table (the receiver already counts
//! bytes; nothing new on the wire), sampled on the GTK main loop by one timer:
//! every [`BUSY_TICK`] while anything moves, every [`IDLE_TICK`] otherwise.
//! Sampling is a short lock on the board and arithmetic, never I/O.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use gtk::prelude::*;
use gtk::{gio, glib};

use crate::device_links::webrtc::transfer_board::{self, TransferRow};

/// The speed is an exponentially weighted average over about this long.
pub(crate) const SMOOTHING: Duration = Duration::from_secs(3);
/// No time left is shown until bytes have moved this long.
pub(crate) const WARM_UP: Duration = Duration::from_secs(2);
/// No bytes this long: stalled, and no time left is shown.
pub(crate) const STALLED: Duration = Duration::from_secs(3);
/// An estimate past this is not one.
pub(crate) const LONGEST_ESTIMATE: Duration = Duration::from_secs(24 * 3600);
/// Samples closer than this are skipped (two readers in one tick).
const MIN_SAMPLE: Duration = Duration::from_millis(200);
/// At most four updates a second per item while something moves.
pub(crate) const BUSY_TICK: Duration = Duration::from_millis(250);
pub(crate) const IDLE_TICK: Duration = Duration::from_secs(1);
/// A finished sync this big, with the window not in front, is announced.
pub(crate) const NOTIFY_FROM_BYTES: u64 = 50_000_000;

// ── Words ──────────────────────────────────────────────────────────────────

/// "820 KiB", "4.5 MiB", "340 MiB", "1.2 GiB": binary units, as the limits
/// (1 GiB a file, 10 GiB in all) are, one decimal below ten, so a number never
/// reads "1024 MiB".
pub(crate) fn size_words(bytes: u64) -> String {
    if bytes < 1024 {
        return format!("{bytes} bytes");
    }
    let units = ["KiB", "MiB", "GiB", "TiB"];
    for (power, unit) in units.iter().enumerate() {
        let value = bytes as f64 / 1024f64.powi(power as i32 + 1);
        let text = if value < 9.95 {
            format!("{value:.1}")
        } else {
            format!("{value:.0}")
        };
        if value < 1023.5 || power == units.len() - 1 {
            return format!("{text} {unit}");
        }
    }
    unreachable!()
}

/// "less than a minute left", "2 min left", "1 h 20 min left".
pub(crate) fn time_left(seconds: u64) -> String {
    let minutes = (seconds + 30) / 60;
    match (seconds, minutes) {
        (0..=59, _) => "less than a minute left".to_string(),
        (_, 0..=59) => format!("{} min left", minutes.max(1)),
        (_, _) => match (minutes / 60, minutes % 60) {
            (hours, 0) => format!("{hours} h left"),
            (hours, minutes) => format!("{hours} h {minutes} min left"),
        },
    }
}

/// "about 4 min left"; under a minute is said plainly.
fn about(seconds: u64) -> String {
    let words = time_left(seconds);
    if seconds < 60 {
        words
    } else {
        format!("about {words}")
    }
}

/// "12 MiB/s".
pub(crate) fn speed_words(bytes_per_second: f64) -> String {
    format!("{}/s", size_words(bytes_per_second.max(0.0).round() as u64))
}

// ── Speed ──────────────────────────────────────────────────────────────────

/// A transfer's speed: an EWMA of the bytes moved between samples, weighted
/// by the time between them, so samples at any rate agree.
#[derive(Debug, Clone)]
pub(crate) struct Rate {
    last: (Instant, u64),
    /// When bytes first moved (since this rate started).
    started: Option<Instant>,
    moved_at: Instant,
    speed: Option<f64>,
}

impl Rate {
    pub(crate) fn new(done: u64, now: Instant) -> Self {
        Self {
            last: (now, done),
            started: None,
            moved_at: now,
            speed: None,
        }
    }

    pub(crate) fn sample(&mut self, done: u64, now: Instant) {
        let (at, before) = self.last;
        let elapsed = now.saturating_duration_since(at);
        if elapsed < MIN_SAMPLE {
            return;
        }
        // Fewer bytes than before: started over.
        if done < before {
            *self = Self::new(done, now);
            return;
        }
        let moved = done - before;
        if moved > 0 {
            self.moved_at = now;
            self.started.get_or_insert(at);
        }
        if self.started.is_some() {
            let instant = moved as f64 / elapsed.as_secs_f64();
            self.speed = Some(match self.speed {
                None => instant,
                Some(speed) => {
                    let weight = 1.0 - (-elapsed.as_secs_f64() / SMOOTHING.as_secs_f64()).exp();
                    speed + weight * (instant - speed)
                }
            });
        }
        self.last = (now, done);
    }

    /// Bytes a second; `None` before anything moved.
    pub(crate) fn speed(&self) -> Option<f64> {
        self.speed
    }

    /// Whether an estimate means anything now: past the warm-up, not stalled.
    fn steady(&self, now: Instant) -> bool {
        self.started
            .is_some_and(|started| now.saturating_duration_since(started) >= WARM_UP)
            && now.saturating_duration_since(self.moved_at) < STALLED
            && self.speed.is_some_and(|speed| speed >= 1.0)
    }

    /// Seconds left for `remaining` bytes, when steady.
    pub(crate) fn eta(&self, remaining: u64, now: Instant) -> Option<u64> {
        if !self.steady(now) {
            return None;
        }
        estimate(remaining, self.speed?)
    }
}

fn estimate(remaining: u64, speed: f64) -> Option<u64> {
    let seconds = (remaining as f64 / speed).ceil();
    (speed >= 1.0 && seconds <= LONGEST_ESTIMATE.as_secs_f64()).then_some(seconds as u64)
}

// ── Per file and per phone ─────────────────────────────────────────────────

/// One Bucket file moving.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Tile {
    pub(crate) done: u64,
    pub(crate) total: u64,
    pub(crate) paused: bool,
    pub(crate) eta: Option<u64>,
}

impl Tile {
    /// "340 MiB of 1.2 GiB · 2 min left", "Waiting…", "Paused".
    pub(crate) fn line(&self) -> String {
        if self.paused {
            return "Paused".to_string();
        }
        if self.done == 0 {
            return "Waiting…".to_string();
        }
        let mut line = format!("{} of {}", size_words(self.done), size_words(self.total));
        if let Some(eta) = self.eta {
            line.push_str(" · ");
            line.push_str(&time_left(eta));
        }
        line
    }

    pub(crate) fn fraction(&self) -> f64 {
        fraction(self.done, self.total)
    }
}

fn fraction(done: u64, total: u64) -> f64 {
    if total == 0 {
        return 0.0;
    }
    (done as f64 / total as f64).clamp(0.0, 1.0)
}

/// A phone's Bucket sync now: every file since it started, finished ones
/// counted whole.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Summary {
    pub(crate) files: usize,
    pub(crate) done: u64,
    pub(crate) total: u64,
    /// Bytes a second, over the files moving.
    pub(crate) speed: f64,
    pub(crate) eta: Option<u64>,
    /// Every file still moving is paused (the link dropped).
    pub(crate) paused: bool,
}

impl Summary {
    /// "Syncing 3 files · 1.1 GiB of 2.4 GiB".
    pub(crate) fn head(&self) -> String {
        let what = match self.files {
            1 => "Syncing 1 file".to_string(),
            files => format!("Syncing {files} files"),
        };
        let what = if self.paused {
            what.replacen("Syncing", "Paused,", 1)
        } else {
            what
        };
        format!(
            "{what} · {} of {}",
            size_words(self.done),
            size_words(self.total)
        )
    }

    /// "Syncing 3 files · 1.1 GiB of 2.4 GiB · about 4 min left".
    pub(crate) fn line(&self) -> String {
        match self.eta {
            Some(eta) if !self.paused => format!("{} · {}", self.head(), about(eta)),
            _ => self.head(),
        }
    }

    /// "12 MB/s · about 4 min left", "12 MB/s", "Waiting…", "Paused".
    pub(crate) fn rate_line(&self) -> String {
        if self.paused {
            return "Paused · resumes when the phone is back".to_string();
        }
        if self.speed < 1.0 {
            return "Waiting…".to_string();
        }
        match self.eta {
            Some(eta) => format!("{} · {}", speed_words(self.speed), about(eta)),
            None => speed_words(self.speed),
        }
    }

    pub(crate) fn fraction(&self) -> f64 {
        fraction(self.done, self.total)
    }
}

/// A sync that ended: what to say about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Ended {
    pub(crate) device: String,
    pub(crate) files: usize,
    pub(crate) incoming: usize,
    pub(crate) bytes: u64,
    /// A file ended without arriving: not "up to date".
    pub(crate) failed: bool,
}

#[derive(Debug, Clone)]
struct Part {
    done: u64,
    total: u64,
    incoming: bool,
    ended: bool,
}

#[derive(Debug, Default)]
struct Batch {
    parts: HashMap<String, Part>,
    failed: bool,
}

/// Every phone's Bucket transfers, as last sampled.
#[derive(Debug, Default)]
pub(crate) struct Tracker {
    rows: Vec<TransferRow>,
    rates: HashMap<String, Rate>,
    batches: HashMap<String, Batch>,
}

/// Files a batch remembers; past it the oldest finished are forgotten.
const MAX_BATCH_FILES: usize = 4096;

impl Tracker {
    /// Takes in the board's rows (and the ids that failed); returns the
    /// syncs that just ended.
    pub(crate) fn sample(
        &mut self,
        rows: Vec<TransferRow>,
        failed: &[String],
        now: Instant,
    ) -> Vec<Ended> {
        self.rates
            .retain(|id, _| rows.iter().any(|row| row.id == *id));
        for row in &rows {
            self.rates
                .entry(row.id.clone())
                .or_insert_with(|| Rate::new(row.done, now))
                .sample(row.done, now);
            let batch = self.batches.entry(row.device_id.clone()).or_default();
            if batch.parts.len() < MAX_BATCH_FILES || batch.parts.contains_key(&row.id) {
                batch.parts.insert(
                    row.id.clone(),
                    Part {
                        done: row.done,
                        total: row.total,
                        incoming: row.incoming,
                        ended: false,
                    },
                );
            }
        }
        let mut ended = Vec::new();
        self.batches.retain(|device, batch| {
            for (id, part) in batch.parts.iter_mut() {
                if part.ended || rows.iter().any(|row| row.id == *id) {
                    continue;
                }
                part.ended = true;
                if failed.contains(id) {
                    batch.failed = true;
                    // Not arrived: it does not count as synced.
                    part.total = part.done;
                } else {
                    part.done = part.total;
                }
            }
            if batch.parts.values().any(|part| !part.ended) {
                return true;
            }
            ended.push(Ended {
                device: device.clone(),
                files: batch.parts.len(),
                incoming: batch.parts.values().filter(|part| part.incoming).count(),
                bytes: batch.parts.values().map(|part| part.total).sum(),
                failed: batch.failed,
            });
            false
        });
        self.rows = rows;
        ended
    }

    /// The Bucket file `path` moving in that direction for `device`, if it is.
    pub(crate) fn tile(
        &self,
        device: &str,
        path: &str,
        incoming: bool,
        now: Instant,
    ) -> Option<Tile> {
        let row = self
            .rows
            .iter()
            .find(|row| row.device_id == device && row.name == path && row.incoming == incoming)?;
        Some(Tile {
            done: row.done,
            total: row.total,
            paused: row.paused,
            eta: self
                .rates
                .get(&row.id)
                .and_then(|rate| rate.eta(row.total.saturating_sub(row.done), now)),
        })
    }

    /// Everything moving under folder `prefix` of `device`'s Bucket, added
    /// up: a folder tile's progress.
    pub(crate) fn folder(&self, device: &str, prefix: &str, now: Instant) -> Option<Tile> {
        let below = format!("{prefix}/");
        let rows: Vec<&TransferRow> = self
            .rows
            .iter()
            .filter(|row| row.device_id == device && row.name.starts_with(&below))
            .collect();
        if rows.is_empty() {
            return None;
        }
        let done: u64 = rows.iter().map(|row| row.done).sum();
        let total: u64 = rows.iter().map(|row| row.total).sum();
        let rates: Vec<&Rate> = rows
            .iter()
            .filter(|row| !row.paused)
            .filter_map(|row| self.rates.get(&row.id))
            .collect();
        let speed: f64 = rates.iter().filter_map(|rate| rate.speed()).sum();
        let steady = rates.iter().any(|rate| rate.steady(now));
        Some(Tile {
            done,
            total,
            paused: rows.iter().all(|row| row.paused),
            eta: steady
                .then(|| estimate(total.saturating_sub(done), speed))
                .flatten(),
        })
    }

    /// `device`'s sync now, while one is going.
    pub(crate) fn summary(&self, device: &str, now: Instant) -> Option<Summary> {
        let batch = self.batches.get(device)?;
        let moving: Vec<&TransferRow> = self
            .rows
            .iter()
            .filter(|row| row.device_id == device)
            .collect();
        if moving.is_empty() {
            return None;
        }
        let rates: Vec<&Rate> = moving
            .iter()
            .filter(|row| !row.paused)
            .filter_map(|row| self.rates.get(&row.id))
            .collect();
        let speed: f64 = rates.iter().filter_map(|rate| rate.speed()).sum();
        let done: u64 = batch.parts.values().map(|part| part.done).sum();
        let total: u64 = batch.parts.values().map(|part| part.total).sum();
        let steady = rates.iter().any(|rate| rate.steady(now));
        Some(Summary {
            files: batch.parts.len(),
            done,
            total,
            speed,
            eta: steady
                .then(|| estimate(total.saturating_sub(done), speed))
                .flatten(),
            paused: moving.iter().all(|row| row.paused),
        })
    }

    fn busy(&self) -> bool {
        !self.rows.is_empty()
    }
}

static TRACKER: Mutex<Option<Tracker>> = Mutex::new(None);

fn with<T>(job: impl FnOnce(&mut Tracker) -> T) -> T {
    let mut tracker = TRACKER
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    job(tracker.get_or_insert_with(Tracker::default))
}

/// The Bucket file `path` (receiving or sending) for `device`, while it moves.
pub(crate) fn tile(device: &str, path: &str, incoming: bool) -> Option<Tile> {
    let now = Instant::now();
    with(|tracker| tracker.tile(device, path, incoming, now))
}

/// The files moving under folder `prefix`, added up.
pub(crate) fn folder(device: &str, prefix: &str) -> Option<Tile> {
    let now = Instant::now();
    with(|tracker| tracker.folder(device, prefix, now))
}

/// `device`'s Bucket sync, while one is going.
pub(crate) fn summary(device: &str) -> Option<Summary> {
    let now = Instant::now();
    with(|tracker| tracker.summary(device, now))
}

/// Tests that use the shared tracker hold this, so they cannot race.
#[cfg(test)]
pub(crate) fn test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Tests elsewhere: one sample of these rows into the shared tracker.
#[cfg(test)]
pub(crate) fn sample_for_test(rows: Vec<TransferRow>) {
    let now = Instant::now();
    with(|tracker| tracker.sample(rows, &[], now));
}

// ── The tick ───────────────────────────────────────────────────────────────

thread_local! {
    /// Called after each sample (the window's live widgets); `false` removes.
    static LISTENERS: RefCell<Vec<Box<dyn Fn() -> bool>>> = RefCell::new(Vec::new());
}

/// Runs `listener` on the main loop after every sample, until it says no.
pub(crate) fn on_tick(listener: impl Fn() -> bool + 'static) {
    LISTENERS.with(|listeners| listeners.borrow_mut().push(Box::new(listener)));
}

thread_local! {
    /// The sampler's wait now pending, and the application it samples for.
    static PENDING: RefCell<Option<glib::SourceId>> = const { RefCell::new(None) };
    static APP: RefCell<Option<glib::WeakRef<crate::application::DesklinkApplication>>> =
        const { RefCell::new(None) };
}

/// Starts sampling, for as long as the application runs.
pub(crate) fn start(app: &crate::application::DesklinkApplication) {
    APP.with(|slot| slot.replace(Some(app.downgrade())));
    // Who is reachable changed, or a window was shown: look now.
    crate::ticker::on_nudge(|| {
        let app = APP.with(|slot| slot.borrow().clone());
        let waiting = PENDING.with(|pending| pending.take());
        if let (Some(app), Some(waiting)) = (app, waiting) {
            waiting.remove();
            schedule(app, Duration::ZERO);
        }
    });
    schedule(app.downgrade(), IDLE_TICK);
}

fn schedule(app: glib::WeakRef<crate::application::DesklinkApplication>, delay: Duration) {
    let id = glib::timeout_add_local_once(delay, move || {
        PENDING.with(|pending| pending.take());
        let Some(strong) = app.upgrade() else {
            return;
        };
        let busy = crate::guard("bucket progress", false, || tick(&strong));
        // Idle: at the pace of the other tickers (every second while a window
        // shows or a phone is reachable, slowly with neither).
        let idle = Duration::from_secs(u64::from(crate::device_links::daemon::tick_seconds()))
            .max(IDLE_TICK);
        schedule(app, if busy { BUSY_TICK } else { idle });
    });
    PENDING.with(|pending| pending.replace(Some(id)));
}

/// One sample: the board, the listeners, the notification. Whether
/// anything is moving.
fn tick(app: &crate::application::DesklinkApplication) -> bool {
    let rows = transfer_board::synced_rows();
    let failed = transfer_board::take_synced_failed();
    let now = Instant::now();
    let (ended, busy) = with(|tracker| {
        let ended = tracker.sample(rows, &failed, now);
        (ended, tracker.busy())
    });
    let listeners = LISTENERS.with(|listeners| listeners.take());
    let kept: Vec<_> = listeners
        .into_iter()
        .filter(|listener| listener())
        .collect();
    LISTENERS.with(|listeners| {
        let mut listeners = listeners.borrow_mut();
        let added = std::mem::take(&mut *listeners);
        *listeners = kept;
        listeners.extend(added);
    });
    // What an add left out (too deep, too big, full…): said in the window
    // when it is in front, else as a notification.
    while let Some(message) = crate::device_links::folders::take_report() {
        let window = app.windows().into_iter().find(|window| window.is_active());
        match window.and_downcast::<crate::DesklinkWindow>() {
            Some(window) => window.show_feature_toast(&message),
            None => {
                let notification = gio::Notification::new("Bucket");
                notification.set_body(Some(&message));
                notification.set_icon(&gio::ThemedIcon::new("derx06.desklink.com-symbolic"));
                app.send_notification(Some("bucket-report"), &notification);
            }
        }
    }
    for sync in ended.iter().filter(|sync| announced(sync)) {
        // Not while the user is looking at DeskLink.
        if app.windows().iter().any(|window| window.is_active()) {
            continue;
        }
        let name = app
            .daemon_handle()
            .and_then(|daemon| {
                daemon
                    .devices()
                    .into_iter()
                    .find(|device| device.id == sync.device)
            })
            .map(|device| device.name)
            .unwrap_or_else(|| "your phone".to_string());
        let notification = gio::Notification::new("Bucket up to date");
        notification.set_body(Some(&ended_words(sync, &name)));
        notification.set_icon(&gio::ThemedIcon::new("derx06.desklink.com-symbolic"));
        notification.add_button_with_target_value(
            "Open Bucket",
            "app.open-bucket",
            Some(&sync.device.to_variant()),
        );
        notification.set_default_action_and_target_value(
            "app.open-bucket",
            Some(&sync.device.to_variant()),
        );
        app.send_notification(
            Some(&format!("bucket-synced-{}", sync.device)),
            &notification,
        );
    }
    busy
}

/// Only a whole, large sync is worth a notification.
fn announced(sync: &Ended) -> bool {
    !sync.failed && sync.bytes >= NOTIFY_FROM_BYTES
}

/// "3 files from Pixel 9", "1 file to Pixel 9".
fn ended_words(sync: &Ended, phone: &str) -> String {
    let files = match sync.files {
        1 => "1 file".to_string(),
        files => format!("{files} files"),
    };
    if sync.incoming > 0 {
        format!("{files} from {phone}")
    } else {
        format!("{files} to {phone}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MB: u64 = 1_000_000;

    fn row(id: &str, done: u64, total: u64) -> TransferRow {
        TransferRow {
            id: id.into(),
            device_id: "phone".into(),
            name: format!("{id}.mov"),
            incoming: true,
            done,
            total,
            preparing: false,
            paused: false,
        }
    }

    /// One size formatter for the whole app (the phone says "MiB" too): no
    /// file shows a size through GLib's decimal one.
    #[test]
    fn no_size_is_shown_in_decimal_units() {
        let needle = ["glib::format", "_size("].concat();
        let mut found = Vec::new();
        let mut stack = vec![std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src")];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(dir).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|ext| ext == "rs")
                    && std::fs::read_to_string(&path).unwrap().contains(&needle)
                {
                    found.push(path);
                }
            }
        }
        assert!(found.is_empty(), "decimal sizes in {found:?}");
    }

    #[test]
    fn sizes_read_in_kib_mib_and_gib() {
        const MIB: u64 = 1 << 20;
        assert_eq!(size_words(0), "0 bytes");
        assert_eq!(size_words(1023), "1023 bytes");
        assert_eq!(size_words(1024), "1.0 KiB");
        assert_eq!(size_words(820 * 1024), "820 KiB");
        assert_eq!(size_words(4_718_592), "4.5 MiB");
        assert_eq!(size_words(10 * MIB - 1), "10 MiB", "never 10.0");
        assert_eq!(size_words(340 * MIB), "340 MiB");
        assert_eq!(size_words(1024 * MIB - 1), "1.0 GiB", "never 1024 MiB");
        assert_eq!(size_words(1_228 * MIB), "1.2 GiB");
        assert_eq!(size_words(24_600 * MIB), "24 GiB");
        assert_eq!(
            size_words(10 << 30),
            "10 GiB",
            "the Bucket's limit reads as itself"
        );
        assert_eq!(speed_words(12.0 * MIB as f64), "12 MiB/s");
    }

    #[test]
    fn time_left_rounds_to_the_minute() {
        assert_eq!(time_left(5), "less than a minute left");
        assert_eq!(time_left(61), "1 min left");
        assert_eq!(time_left(95), "2 min left");
        assert_eq!(time_left(3_599), "1 h left");
        assert_eq!(time_left(4_800), "1 h 20 min left");
        assert_eq!(about(240), "about 4 min left");
        assert_eq!(about(20), "less than a minute left");
    }

    #[test]
    fn a_tile_reads_done_of_total_and_time_left() {
        let tile = Tile {
            done: 340 * MB,
            total: 1_200 * MB,
            paused: false,
            eta: Some(118),
        };
        assert_eq!(
            tile.line(),
            format!(
                "{} of {} · 2 min left",
                size_words(340 * MB),
                size_words(1_200 * MB)
            )
        );
        assert_eq!(
            Tile {
                eta: None,
                ..tile.clone()
            }
            .line(),
            format!("{} of {}", size_words(340 * MB), size_words(1_200 * MB))
        );
        assert_eq!(
            Tile {
                done: 0,
                ..tile.clone()
            }
            .line(),
            "Waiting…"
        );
        assert_eq!(
            Tile {
                paused: true,
                ..tile.clone()
            }
            .line(),
            "Paused"
        );
        assert!((tile.fraction() - 340.0 / 1200.0).abs() < 1e-9);
        assert_eq!(Tile { total: 0, ..tile }.fraction(), 0.0);
    }

    #[test]
    fn the_speed_is_smoothed_warms_up_and_hides_on_a_stall() {
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        let mut rate = Rate::new(0, start);
        rate.sample(0, at(250));
        assert_eq!(rate.speed(), None, "nothing moved yet");
        // 10 MB/s, sampled four times a second.
        for tick in 2..=8 {
            rate.sample((tick - 1) * 2_500_000, at(tick * 250));
        }
        assert!((rate.speed().unwrap() - 10e6).abs() < 1.0);
        assert_eq!(rate.eta(100 * MB, at(2_000)), None, "warming up");
        for tick in 9..=16 {
            rate.sample((tick - 1) * 2_500_000, at(tick * 250));
        }
        assert_eq!(rate.eta(100 * MB, at(4_000)), Some(10));
        // One fast burst moves the average a little, not all the way.
        rate.sample(37_500_000 + 25_000_000, at(4_250));
        let speed = rate.speed().unwrap();
        assert!(speed > 10e6 && speed < 20e6, "{speed}");
        // A stall: the speed decays and, past STALLED, there is no estimate.
        for tick in 18..=48 {
            rate.sample(62_500_000, at(tick * 250));
        }
        assert!(rate.speed().unwrap() < 2e6);
        assert_eq!(rate.eta(100 * MB, at(12_000)), None, "stalled");
        // Moving again: an estimate once steady.
        for tick in 49..=68 {
            rate.sample(62_500_000 + (tick - 48) * 2_500_000, at(tick * 250));
        }
        assert!(rate.eta(100 * MB, at(17_000)).is_some());
        // Started over (fewer bytes): a fresh rate.
        rate.sample(0, at(17_250));
        assert_eq!(rate.speed(), None);
    }

    #[test]
    fn the_header_adds_up_every_file_of_the_sync() {
        let start = Instant::now();
        let at = |s: u64| start + Duration::from_secs(s);
        let mut tracker = Tracker::default();
        assert!(tracker
            .sample(
                vec![
                    row("a", 0, 600 * MB),
                    row("b", 0, 1_000 * MB),
                    row("c", 0, 800 * MB)
                ],
                &[],
                at(0)
            )
            .is_empty());
        let summary = tracker.summary("phone", at(0)).unwrap();
        assert_eq!(
            summary.head(),
            format!("Syncing 3 files · 0 bytes of {}", size_words(2_400 * MB))
        );
        assert_eq!(summary.rate_line(), "Waiting…");
        // Each file at 2 MB/s for 10 s; "a" then finishes.
        for second in 1..=10 {
            tracker.sample(
                vec![
                    row("a", second * 2 * MB, 600 * MB),
                    row("b", second * 2 * MB, 1_000 * MB),
                    row("c", second * 2 * MB, 800 * MB),
                ],
                &[],
                at(second),
            );
        }
        let summary = tracker.summary("phone", at(10)).unwrap();
        assert_eq!(summary.done, 60 * MB);
        assert!((summary.speed - 6e6).abs() < 1.0);
        assert_eq!(summary.eta, Some(390));
        assert_eq!(
            summary.line(),
            format!(
                "Syncing 3 files · {} of {} · about 7 min left",
                size_words(60 * MB),
                size_words(2_400 * MB)
            )
        );
        assert_eq!(
            summary.rate_line(),
            format!("{} · about 7 min left", speed_words(6.0 * MB as f64))
        );
        let ended = tracker.sample(
            vec![row("b", 22 * MB, 1_000 * MB), row("c", 22 * MB, 800 * MB)],
            &[],
            at(11),
        );
        assert!(ended.is_empty());
        let summary = tracker.summary("phone", at(11)).unwrap();
        assert_eq!(
            (summary.files, summary.done, summary.total),
            (3, 644 * MB, 2_400 * MB),
            "a counts whole"
        );
        // The link drops: paused.
        let paused = |id: &str, done| TransferRow {
            paused: true,
            ..row(id, done, 1_000 * MB)
        };
        tracker.sample(
            vec![paused("b", 22 * MB), paused("c", 22 * MB)],
            &[],
            at(12),
        );
        let summary = tracker.summary("phone", at(12)).unwrap();
        assert!(summary.paused);
        assert_eq!(
            summary.line(),
            format!(
                "Paused, 3 files · {} of {}",
                size_words(644 * MB),
                size_words(2_600 * MB)
            )
        );
        // Everything ends: one Ended, and no summary.
        let ended = tracker.sample(Vec::new(), &[], at(13));
        assert_eq!(
            ended,
            [Ended {
                device: "phone".into(),
                files: 3,
                incoming: 3,
                bytes: 2_600 * MB,
                failed: false
            }]
        );
        assert!(tracker.summary("phone", at(13)).is_none());
        assert!(announced(&ended[0]));
        assert_eq!(ended_words(&ended[0], "Pixel 9"), "3 files from Pixel 9");
    }

    #[test]
    fn a_folder_adds_up_what_moves_inside_it() {
        let start = Instant::now();
        let mut tracker = Tracker::default();
        let named = |id: &str, name: &str, done, total| TransferRow {
            name: name.into(),
            ..row(id, done, total)
        };
        for second in 0..=4u64 {
            tracker.sample(
                vec![
                    named("a", "Trips/a.mov", second * 4 * MB, 100 * MB),
                    named("b", "Trips/Rome/b.mov", second * 2 * MB, 50 * MB),
                    named("c", "Other/c.mov", 1, 10),
                ],
                &[],
                start + Duration::from_secs(second),
            );
        }
        let now = start + Duration::from_secs(4);
        let trips = tracker.folder("phone", "Trips", now).unwrap();
        assert_eq!(
            (trips.done, trips.total),
            (24 * MB, 150 * MB),
            "both, not Other"
        );
        assert_eq!(trips.eta, Some(21), "126 MB at 6 MB/s (any unit)");
        assert_eq!(
            tracker.folder("phone", "Trips/Rome", now).unwrap().total,
            50 * MB
        );
        assert_eq!(
            tracker.folder("phone", "Trip", now),
            None,
            "not a prefix match"
        );
        assert_eq!(tracker.folder("other-phone", "Trips", now), None);
        assert!(trips.line().starts_with(&format!(
            "{} of {}",
            size_words(24 * MB),
            size_words(150 * MB)
        )));
    }

    #[test]
    fn small_or_failed_syncs_are_not_announced() {
        let mut tracker = Tracker::default();
        let now = Instant::now();
        tracker.sample(vec![row("small", 0, 10 * MB)], &[], now);
        let ended = tracker.sample(Vec::new(), &[], now);
        assert!(!announced(&ended[0]), "under 50 MB");
        tracker.sample(vec![row("big", 0, 900 * MB)], &[], now);
        let ended = tracker.sample(Vec::new(), &["big".to_string()], now);
        assert!(ended[0].failed && !announced(&ended[0]));
        assert_eq!(
            ended_words(
                &Ended {
                    incoming: 0,
                    files: 1,
                    ..ended[0].clone()
                },
                "Pixel 9"
            ),
            "1 file to Pixel 9"
        );
    }
}
