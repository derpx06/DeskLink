//! What the user can see of file transfers: a live table and a queue of
//! outcomes.
//!
//! The transfer manager writes here as chunks move; the GTK side only reads.
//! Neither waits on the other, and nothing here touches the disk or the
//! network. It is a process-wide table, like the phone-file browser's, because
//! a transfer outlives any one window and the manager has no handle to the UI.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, OnceLock};

/// Rows are bounded by the manager's own limits; this only stops a bug there
/// from growing the table without end.
const MAX_ROWS: usize = 64;
/// Outcomes the UI has not collected yet. Oldest are dropped first.
const MAX_FINISHED: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferRow {
    pub id: String,
    pub device_id: String,
    pub name: String,
    pub incoming: bool,
    /// Bytes the other side has confirmed (sending) or this side has written
    /// (receiving).
    pub done: u64,
    pub total: u64,
    /// A file being copied and hashed before its offer can be made. It has no
    /// size to show progress against yet.
    pub preparing: bool,
    /// The connection dropped: both sides keep their checkpoint, and the
    /// transfer resumes when the device reconnects.
    pub paused: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// An incoming file now at this path.
    Received(PathBuf),
    Sent,
    Failed(String),
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finished {
    pub id: String,
    pub device_id: String,
    pub name: String,
    pub incoming: bool,
    pub outcome: Outcome,
}

#[derive(Debug, Default)]
pub struct TransferBoard {
    /// Insertion order, so rows do not jump around as they update.
    rows: Vec<TransferRow>,
    finished: VecDeque<Finished>,
    /// Bucket files moving (F8b): not downloads, so not on the Transfers
    /// page and never in the history. `name` is the path in the Bucket.
    synced: Vec<TransferRow>,
    /// Bucket transfers that ended without their file: the sync is not done.
    synced_failed: VecDeque<String>,
    /// Every Bucket transfer that ended, for the folder worker's slots.
    synced_ended: VecDeque<SyncedEnd>,
}

/// A Bucket transfer that ended (F8c: frees an in-flight slot).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncedEnd {
    pub device: String,
    /// The path in the Bucket.
    pub name: String,
    pub incoming: bool,
    /// It arrived (or was delivered).
    pub ok: bool,
}

impl TransferBoard {
    /// Adds a transfer, or replaces the row of one already listed (a resumed
    /// transfer is offered again under the same id).
    pub fn begin(&mut self, row: TransferRow) {
        if let Some(existing) = self.rows.iter_mut().find(|known| known.id == row.id) {
            *existing = row;
            return;
        }
        if self.rows.len() >= MAX_ROWS {
            self.rows.remove(0);
        }
        self.rows.push(row);
    }

    /// Adds (or replaces) a Bucket transfer.
    pub fn begin_synced(&mut self, row: TransferRow) {
        // The same file restarted under a new id (it changed, or was offered
        // again): the old row is superseded, not left paused for ever.
        self.synced.retain(|known| {
            known.id == row.id
                || !(known.device_id == row.device_id
                    && known.name == row.name
                    && known.incoming == row.incoming)
        });
        if let Some(existing) = self.synced.iter_mut().find(|known| known.id == row.id) {
            *existing = row;
            return;
        }
        if self.synced.len() >= MAX_ROWS {
            self.synced.remove(0);
        }
        self.synced.push(row);
    }

    /// Reports a transfer that ended without ever being listed, such as an
    /// offer refused on arrival.
    pub fn record(&mut self, finished: Finished) {
        if self.finished.len() >= MAX_FINISHED {
            self.finished.pop_front();
        }
        self.finished.push_back(finished);
    }

    /// Records how far a listed transfer has got; unknown ids are ignored.
    pub fn progress(&mut self, id: &str, done: u64) {
        if let Some(row) = self
            .rows
            .iter_mut()
            .chain(self.synced.iter_mut())
            .find(|row| row.id == id)
        {
            row.preparing = false;
            row.paused = false;
            row.done = if row.total > 0 {
                done.min(row.total)
            } else {
                done
            };
        }
    }

    /// Ends a listed transfer. A transfer that is not listed is ignored, so two
    /// paths that both notice the end (a peer's `Error` and our own cleanup)
    /// report it once.
    pub fn finish(&mut self, id: &str, outcome: Outcome) {
        if let Some(position) = self.synced.iter().position(|row| row.id == id) {
            let row = self.synced.remove(position);
            if self.synced_ended.len() >= MAX_FINISHED {
                self.synced_ended.pop_front();
            }
            self.synced_ended.push_back(SyncedEnd {
                device: row.device_id,
                name: row.name,
                incoming: row.incoming,
                ok: matches!(outcome, Outcome::Sent | Outcome::Received(_)),
            });
            if matches!(outcome, Outcome::Failed(_) | Outcome::Cancelled) {
                if self.synced_failed.len() >= MAX_FINISHED {
                    self.synced_failed.pop_front();
                }
                self.synced_failed.push_back(id.to_string());
            }
            return;
        }
        let Some(position) = self.rows.iter().position(|row| row.id == id) else {
            return;
        };
        let row = self.rows.remove(position);
        if self.finished.len() >= MAX_FINISHED {
            self.finished.pop_front();
        }
        self.finished.push_back(Finished {
            id: row.id,
            device_id: row.device_id,
            name: row.name,
            incoming: row.incoming,
            outcome,
        });
    }

    /// Pauses everything a device had running when its connection drops:
    /// the rows stay ("Paused — resumes when … reconnects") and nothing is
    /// reported as failed. A resumed transfer is offered again under the
    /// same id and replaces its row.
    pub fn pause_device(&mut self, device_id: &str) {
        for row in self
            .rows
            .iter_mut()
            .chain(self.synced.iter_mut())
            .filter(|row| row.device_id == device_id)
        {
            row.paused = true;
        }
    }

    /// Ends everything a device had running.
    pub fn finish_device(&mut self, device_id: &str, reason: &str) {
        let ids: Vec<String> = self
            .rows
            .iter()
            .chain(self.synced.iter())
            .filter(|row| row.device_id == device_id)
            .map(|row| row.id.clone())
            .collect();
        for id in ids {
            self.finish(&id, Outcome::Failed(reason.to_string()));
        }
    }

    pub fn rows(&self) -> Vec<TransferRow> {
        self.rows.clone()
    }

    pub fn take_finished(&mut self) -> Vec<Finished> {
        self.finished.drain(..).collect()
    }
}

fn board() -> MutexGuard<'static, TransferBoard> {
    static BOARD: OnceLock<Mutex<TransferBoard>> = OnceLock::new();
    // A panic elsewhere must not blank the user's view of their transfers.
    BOARD
        .get_or_init(|| Mutex::new(TransferBoard::default()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub fn begin(row: TransferRow) {
    board().begin(row);
}

pub fn progress(id: &str, done: u64) {
    board().progress(id, done);
}

pub fn finish(id: &str, outcome: Outcome) {
    let received = match &outcome {
        Outcome::Received(path) => board()
            .rows
            .iter()
            .find(|row| row.id == id)
            .map(|row| (row.device_id.clone(), row.name.clone(), path.clone())),
        _ => None,
    };
    board().finish(id, outcome);
    // Outside the board's lock: this writes a file.
    if let Some((device_id, name, path)) = received {
        crate::device_links::transfer_log::record_received(&device_id, &name, &path);
    }
}

pub fn record(finished: Finished) {
    board().record(finished);
}

pub fn finish_device(device_id: &str, reason: &str) {
    board().finish_device(device_id, reason);
}

pub fn pause_device(device_id: &str) {
    board().pause_device(device_id);
}

pub fn rows() -> Vec<TransferRow> {
    board().rows()
}

pub fn begin_synced(row: TransferRow) {
    board().begin_synced(row);
}

/// Bucket transfers moving now.
pub fn synced_rows() -> Vec<TransferRow> {
    board().synced.clone()
}

/// Bucket transfers that ended since last asked.
pub fn take_synced_ended() -> Vec<SyncedEnd> {
    board().synced_ended.drain(..).collect()
}

/// Bucket transfers that ended without their file since last asked.
pub fn take_synced_failed() -> Vec<String> {
    board().synced_failed.drain(..).collect()
}

pub fn take_finished() -> Vec<Finished> {
    board().take_finished()
}

/// The outcomes reported for `id` so far, without collecting them: tests share
/// this table, and draining it would steal each other's results.
#[cfg(test)]
pub fn peek_finished(id: &str) -> Vec<Finished> {
    board()
        .finished
        .iter()
        .filter(|item| item.id == id)
        .cloned()
        .collect()
}

/// The listed row for `id`, if any.
#[cfg(test)]
pub fn peek_row(id: &str) -> Option<TransferRow> {
    board().rows.iter().find(|row| row.id == id).cloned()
}

/// Lists a file as "being prepared" until dropped.
///
/// Copying and hashing a large file takes long enough that the user needs to
/// see something happen. The real transfer replaces this row under its own id
/// once the offer exists; whatever the outcome, this one goes.
pub struct Preparing {
    id: String,
}

impl Preparing {
    pub fn new(device_id: &str, name: &str) -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let id = format!(
            "preparing-{}",
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        begin(TransferRow {
            id: id.clone(),
            device_id: device_id.to_string(),
            name: name.to_string(),
            incoming: false,
            done: 0,
            total: 0,
            preparing: true,
            paused: false,
        });
        Self { id }
    }
}

impl Drop for Preparing {
    fn drop(&mut self) {
        let mut board = board();
        // Not `finish`: a file that became a real transfer, or failed, has its
        // own outcome, and this placeholder must not add a second one.
        board.rows.retain(|row| row.id != self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, device: &str, incoming: bool, total: u64) -> TransferRow {
        TransferRow {
            id: id.into(),
            device_id: device.into(),
            name: format!("{id}.bin"),
            incoming,
            done: 0,
            total,
            preparing: false,
            paused: false,
        }
    }

    #[test]
    fn a_transfer_is_listed_advances_and_ends_once() {
        let mut board = TransferBoard::default();
        board.begin(row("t1", "phone", true, 100));
        board.progress("t1", 40);
        assert_eq!(board.rows()[0].done, 40);

        board.finish("t1", Outcome::Received(PathBuf::from("/tmp/t1.bin")));
        board.finish("t1", Outcome::Failed("late".into()));

        assert!(board.rows().is_empty());
        let finished = board.take_finished();
        assert_eq!(finished.len(), 1, "a second end must not be reported");
        assert_eq!(finished[0].name, "t1.bin");
        assert!(finished[0].incoming);
        assert_eq!(
            finished[0].outcome,
            Outcome::Received(PathBuf::from("/tmp/t1.bin"))
        );
        assert!(
            board.take_finished().is_empty(),
            "outcomes are collected once"
        );
    }

    #[test]
    fn progress_never_passes_the_total_and_ignores_unknown_transfers() {
        let mut board = TransferBoard::default();
        board.begin(row("t1", "phone", false, 100));
        board.progress("t1", 250);
        board.progress("ghost", 5);
        assert_eq!(board.rows().len(), 1);
        assert_eq!(board.rows()[0].done, 100);
    }

    #[test]
    fn a_resumed_transfer_replaces_its_row_instead_of_doubling_it() {
        let mut board = TransferBoard::default();
        board.begin(row("t1", "phone", true, 100));
        board.progress("t1", 60);
        board.begin(row("t1", "phone", true, 100));
        assert_eq!(board.rows().len(), 1);
        assert_eq!(board.rows()[0].done, 0);
    }

    #[test]
    fn rows_keep_their_order_as_they_update() {
        let mut board = TransferBoard::default();
        for id in ["a", "b", "c"] {
            board.begin(row(id, "phone", true, 10));
        }
        board.progress("a", 5);
        board.finish("b", Outcome::Sent);
        let ids: Vec<_> = board.rows().into_iter().map(|row| row.id).collect();
        assert_eq!(ids, ["a", "c"]);
    }

    #[test]
    fn a_dropped_connection_pauses_its_transfers_until_they_resume() {
        let mut board = TransferBoard::default();
        board.begin(row("p", "phone", true, 10));
        board.begin(row("t", "tablet", true, 10));
        board.pause_device("phone");
        assert!(board.take_finished().is_empty(), "no failure is reported");
        let paused: Vec<_> = board
            .rows()
            .into_iter()
            .map(|row| (row.id, row.paused))
            .collect();
        assert_eq!(paused, [("p".into(), true), ("t".into(), false)]);
        // The resumed offer replaces the row; progress also clears it.
        board.begin(row("p", "phone", true, 10));
        assert!(!board.rows()[0].paused);
        board.pause_device("phone");
        board.progress("p", 4);
        assert!(!board.rows()[0].paused);
    }

    #[test]
    fn losing_a_device_fails_only_its_transfers() {
        let mut board = TransferBoard::default();
        board.begin(row("a", "phone", true, 10));
        board.begin(row("b", "tablet", true, 10));
        board.finish_device("phone", "Connection lost");

        let ids: Vec<_> = board.rows().into_iter().map(|row| row.id).collect();
        assert_eq!(ids, ["b"]);
        let finished = board.take_finished();
        assert_eq!(finished.len(), 1);
        assert_eq!(
            finished[0].outcome,
            Outcome::Failed("Connection lost".into())
        );
    }

    #[test]
    fn neither_table_grows_without_bound() {
        let mut board = TransferBoard::default();
        for index in 0..(MAX_ROWS + 10) {
            board.begin(row(&format!("t{index}"), "phone", true, 10));
        }
        assert_eq!(board.rows().len(), MAX_ROWS);
        for index in 0..(MAX_FINISHED + 10) {
            board.begin(row(&format!("f{index}"), "phone", true, 10));
            board.finish(&format!("f{index}"), Outcome::Sent);
        }
        let finished = board.take_finished();
        assert_eq!(finished.len(), MAX_FINISHED);
        assert_eq!(
            finished.last().map(|item| item.id.as_str()),
            Some(format!("f{}", MAX_FINISHED + 9).as_str()),
            "the newest outcome survives"
        );
    }

    #[test]
    fn bucket_transfers_move_pause_and_end_off_the_transfers_page() {
        let mut board = TransferBoard::default();
        board.begin_synced(row("s1", "phone", true, 100));
        board.begin_synced(row("s2", "phone", false, 100));
        board.progress("s1", 40);
        assert!(board.rows().is_empty(), "not a download");
        assert_eq!(board.synced[0].done, 40);
        board.pause_device("phone");
        assert!(board.synced.iter().all(|row| row.paused));
        board.progress("s1", 50);
        assert!(!board.synced[0].paused, "moving again");
        board.finish("s1", Outcome::Sent);
        board.finish("s2", Outcome::Failed("gone".into()));
        assert!(board.synced.is_empty());
        assert!(board.take_finished().is_empty(), "never in the history");
        assert_eq!(board.synced_failed, ["s2"]);
    }

    #[test]
    fn a_bucket_transfer_restarted_under_a_new_id_replaces_its_old_row() {
        let mut board = TransferBoard::default();
        let named = |id: &str, name: &str, incoming| TransferRow {
            name: name.into(),
            ..row(id, "phone", incoming, 100)
        };
        board.begin_synced(named("old", "A/f.mov", true));
        board.begin_synced(named("other-way", "A/f.mov", false));
        board.begin_synced(named("other-file", "A/g.mov", true));
        board.pause_device("phone");
        board.begin_synced(named("new", "A/f.mov", true));
        let ids: Vec<&str> = board.synced.iter().map(|row| row.id.as_str()).collect();
        assert_eq!(ids, ["other-way", "other-file", "new"], "{ids:?}");
        assert!(
            !board
                .synced
                .iter()
                .find(|row| row.id == "new")
                .unwrap()
                .paused
        );
        assert!(board.synced_ended.is_empty(), "superseded is not an ending");
        assert!(board.synced_failed.is_empty());
    }

    #[test]
    fn a_preparing_row_appears_and_disappears_without_an_outcome() {
        let name = "preparing-test-unique.bin";
        let guard = Preparing::new("phone-preparing-test", name);
        assert!(rows()
            .iter()
            .any(|row| row.name == name && row.preparing && row.total == 0));
        drop(guard);
        assert!(!rows().iter().any(|row| row.name == name));
        assert!(!board().finished.iter().any(|item| item.name == name));
    }
}
