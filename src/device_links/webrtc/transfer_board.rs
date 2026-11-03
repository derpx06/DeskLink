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
/// Rows kept free of "preparing" placeholders, for real transfers.
const PLACEHOLDER_RESERVE: usize = 12;
/// Outcomes the UI has not collected yet. Oldest are dropped first. (Tests
/// share this queue across threads and never collect it.)
#[cfg(not(test))]
const MAX_FINISHED: usize = 64;
#[cfg(test)]
const MAX_FINISHED: usize = 8192;

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
    /// For a send that failed: the file, so the person can try again.
    pub retry: Option<PathBuf>,
}

#[derive(Debug, Default)]
pub struct TransferBoard {
    /// Insertion order, so rows do not jump around as they update.
    rows: Vec<TransferRow>,
    finished: VecDeque<Finished>,
    retry_sources: std::collections::HashMap<String, PathBuf>,
    /// Bucket files moving (F8b): not downloads, so not on the Transfers
    /// page and never in the history. `name` is the path in the Bucket.
    synced: Vec<TransferRow>,
    /// Bucket transfers that ended without their file: the sync is not done.
    synced_failed: VecDeque<String>,
    /// Every Bucket transfer that ended, for the folder worker's slots.
    synced_ended: VecDeque<SyncedEnd>,
    /// Why a listed transfer is not moving right now, in the person's words
    /// ("Storage is full. The transfer continues when there is space."), by
    /// transfer id. Dropped when the transfer ends.
    notes: std::collections::HashMap<String, String>,
    /// The last outcomes, kept (not drained) for a second reader that wants
    /// to follow them: the desktop notification. Numbered from 1.
    ended: VecDeque<(u64, Finished)>,
    ended_seq: u64,
    /// Which lane a listed transfer is on, in the person's words ("fast
    /// lane", "over the secure link"), by transfer id.
    lanes: std::collections::HashMap<String, &'static str>,
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
    ///
    /// A live transfer is never pushed off the board to make room: a full
    /// board gives up a "preparing" placeholder (cosmetic) instead, and when
    /// there is none the new row is refused (`false`).
    pub fn begin(&mut self, row: TransferRow) -> bool {
        if let Some(existing) = self.rows.iter_mut().find(|known| known.id == row.id) {
            *existing = row;
            return true;
        }
        if self.rows.len() >= MAX_ROWS {
            let Some(placeholder) = self.rows.iter().position(|known| known.preparing) else {
                return false;
            };
            if row.preparing {
                return false;
            }
            self.rows.remove(placeholder);
        }
        self.rows.push(row);
        true
    }

    /// Says which lane a listed transfer is on (`None`: not said). A transfer
    /// that is not listed has none: the map holds only what the board lists.
    pub fn set_lane(&mut self, id: &str, lane: Option<&'static str>) {
        match lane {
            Some(lane) if self.rows.iter().chain(&self.synced).any(|row| row.id == id) => {
                self.lanes.insert(id.to_string(), lane);
            }
            _ => {
                self.lanes.remove(id);
            }
        }
    }

    /// Drops the lane of every transfer that is no longer listed (a row that
    /// was superseded, evicted or discarded never reaches `finish`).
    fn prune_lanes(&mut self) {
        let (rows, synced) = (&self.rows, &self.synced);
        self.lanes
            .retain(|id, _| rows.iter().chain(synced).any(|row| row.id == *id));
    }

    /// Whether a real transfer can be listed now.
    pub fn has_room(&self) -> bool {
        self.rows.len() < MAX_ROWS || self.rows.iter().any(|row| row.preparing)
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
            self.prune_lanes();
            return;
        }
        if self.synced.len() >= MAX_ROWS {
            // Pushed off the list, not forgotten: the Bucket's slot for it is
            // freed and the file counts as not synced (it is asked for again).
            let evicted = self.synced.remove(0);
            self.notes.remove(&evicted.id);
            if self.synced_ended.len() >= MAX_FINISHED {
                self.synced_ended.pop_front();
            }
            self.synced_ended.push_back(SyncedEnd {
                device: evicted.device_id,
                name: evicted.name,
                incoming: evicted.incoming,
                ok: false,
            });
            if self.synced_failed.len() >= MAX_FINISHED {
                self.synced_failed.pop_front();
            }
            self.synced_failed.push_back(evicted.id);
        }
        self.synced.push(row);
        self.prune_lanes();
    }

    /// Reports a transfer that ended without ever being listed, such as an
    /// offer refused on arrival.
    pub fn record(&mut self, finished: Finished) {
        self.push_finished(finished);
    }

    fn push_finished(&mut self, finished: Finished) {
        #[cfg(test)]
        test_log()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(finished.clone());
        self.ended_seq += 1;
        if self.ended.len() >= 64 {
            self.ended.pop_front();
        }
        self.ended.push_back((self.ended_seq, finished.clone()));
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
        self.notes.remove(id);
        self.lanes.remove(id);
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
        let retry = self
            .retry_sources
            .remove(&row.id)
            .filter(|_| matches!(outcome, Outcome::Failed(_)));
        self.push_finished(Finished {
            id: row.id,
            device_id: row.device_id,
            name: row.name,
            incoming: row.incoming,
            outcome,
            retry,
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
            // Not on any lane while the connection is gone.
            self.lanes.remove(&row.id);
        }
    }

    /// Ends everything a device had running. Returns the transfers ended. A
    /// failed row of a device that is gone offers no "Try Again".
    pub fn finish_device(&mut self, device_id: &str, reason: &str) -> Vec<String> {
        let ids: Vec<String> = self
            .rows
            .iter()
            .chain(self.synced.iter())
            .filter(|row| row.device_id == device_id)
            .map(|row| row.id.clone())
            .collect();
        for id in &ids {
            self.retry_sources.remove(id);
            self.finish(id, Outcome::Failed(reason.to_string()));
        }
        ids
    }

    pub fn rows(&self) -> Vec<TransferRow> {
        self.rows.clone()
    }

    /// Removes a listed transfer without recording an outcome: it carries on
    /// under another id (a changed file begun again).
    pub fn discard(&mut self, id: &str) {
        self.notes.remove(id);
        self.lanes.remove(id);
        self.retry_sources.remove(id);
        self.rows.retain(|row| row.id != id);
        self.synced.retain(|row| row.id != id);
    }

    /// Says why a transfer is waiting (`None`: it is moving again).
    pub fn set_note(&mut self, id: &str, note: Option<String>) {
        match note {
            Some(note) if self.rows.iter().chain(&self.synced).any(|row| row.id == id) => {
                self.notes.insert(id.to_string(), note);
            }
            _ => {
                self.notes.remove(id);
            }
        }
    }

    /// Remembers the file of a send, for a "Try Again" if it fails.
    pub fn set_retry_source(&mut self, id: &str, path: PathBuf) {
        if self.retry_sources.len() >= MAX_FINISHED {
            if let Some(oldest) = self.retry_sources.keys().next().cloned() {
                self.retry_sources.remove(&oldest);
            }
        }
        self.retry_sources.insert(id.to_string(), path);
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

pub fn begin(row: TransferRow) -> bool {
    board().begin(row)
}

/// Whether one more real transfer can be listed (a send or an offer is
/// refused in words when not, instead of running unseen and uncancellable).
pub fn has_room() -> bool {
    board().has_room()
}

pub fn progress(id: &str, done: u64) {
    board().progress(id, done);
}

/// Remembers the file of the send `id`, so a failure can offer to try again.
pub fn set_retry_source(id: &str, path: PathBuf) {
    board().set_retry_source(id, path);
}

pub fn finish(id: &str, outcome: Outcome) {
    // A lane token of a transfer that is over opens nothing (W18).
    super::file_lane::void_tokens(id);
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
    let ended = board().finish_device(device_id, reason);
    // The lane tokens of what ended open nothing (W18), as in `finish`.
    for id in ended {
        super::file_lane::void_tokens(&id);
    }
}

pub fn pause_device(device_id: &str) {
    board().pause_device(device_id);
}

/// Removes a listed transfer without an outcome (see [`TransferBoard::discard`]).
pub fn discard(id: &str) {
    board().discard(id);
}

/// Says why a listed transfer is waiting; `None` clears it.
pub fn set_note(id: &str, note: Option<&str>) {
    board().set_note(id, note.map(str::to_string));
}

/// The outcomes recorded after number `seen` (0: all kept), and the number to
/// ask from next time. Nothing is drained: the window's own queue is separate.
pub fn ended_since(seen: u64) -> (Vec<Finished>, u64) {
    let board = board();
    let news = board
        .ended
        .iter()
        .filter(|(seq, _)| *seq > seen)
        .map(|(_, finished)| finished.clone())
        .collect();
    (news, board.ended_seq)
}

/// The listed transfers and the outcomes after number `seen`, read together
/// under one lock: a transfer that ends between two separate reads would have
/// left the list without its outcome in hand. The number to ask from next time.
pub fn snapshot(seen: u64) -> (Vec<TransferRow>, Vec<Finished>, u64) {
    let board = board();
    let news = board
        .ended
        .iter()
        .filter(|(seq, _)| *seq > seen)
        .map(|(_, finished)| finished.clone())
        .collect();
    (board.rows(), news, board.ended_seq)
}

/// Says which lane `id` is on now (`None`: not said).
pub fn set_lane(id: &str, lane: Option<&'static str>) {
    board().set_lane(id, lane);
}

/// Every listed transfer's lane, for the notification.
pub fn lanes() -> std::collections::HashMap<String, &'static str> {
    board().lanes.clone()
}

/// The lane `id` is on, in words.
pub fn lane_of(id: &str) -> Option<&'static str> {
    board().lanes.get(id).copied()
}

/// Why `id` is waiting, if it is.
pub fn note(id: &str) -> Option<String> {
    board().notes.get(id).cloned()
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
    test_log()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .iter()
        .filter(|item| item.id == id)
        .cloned()
        .collect()
}

/// Every outcome ever recorded in this test run, never drained or evicted:
/// the daemon's event drain (`take_finished`) and the history cap used to take
/// a result away from the test that was waiting for it.
#[cfg(test)]
fn test_log() -> &'static Mutex<Vec<Finished>> {
    static LOG: OnceLock<Mutex<Vec<Finished>>> = OnceLock::new();
    LOG.get_or_init(Default::default)
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
    /// The row's id, to say why it waits (see [`set_note`]).
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn new(device_id: &str, name: &str) -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let id = format!(
            "preparing-{}",
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        // Placeholders leave room for real transfers: past this many they
        // are not listed at all (the file is still sent).
        if board().rows.len() >= MAX_ROWS - PLACEHOLDER_RESERVE {
            return Self { id };
        }
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

    /// A transfer that is over takes its lane tokens with it, and says no lane.
    #[test]
    fn finishing_a_transfer_voids_its_lane_token_and_forgets_its_lane() {
        let id = "lane-board-1";
        let token = super::super::file_lane::issue_token(id, "phone").unwrap();
        assert!(begin(row(id, "lane-dev-1", false, 9 << 20)));
        set_lane(id, Some("fast, over Wi-Fi"));
        assert_eq!(lane_of(id), Some("fast, over Wi-Fi"));
        assert_eq!(lanes().get(id).copied(), Some("fast, over Wi-Fi"));
        finish(id, Outcome::Sent);
        assert_eq!(lane_of(id), None);
        assert!(!super::super::file_lane::spend_token_for_test(
            &token, id, "phone"
        ));
        // A token of a transfer still going is good once.
        let live = super::super::file_lane::issue_token("lane-board-2", "phone").unwrap();
        assert!(super::super::file_lane::spend_token_for_test(
            &live,
            "lane-board-2",
            "phone"
        ));
    }

    /// Lanes belong to listed transfers only: whatever way a transfer leaves
    /// the board (ended, discarded, superseded, evicted, its device gone or
    /// paused), nothing is left in the map. A thousand, created and aborted.
    #[test]
    fn the_lane_map_returns_to_empty_however_a_thousand_transfers_end() {
        let mut board = TransferBoard::default();
        // Not listed: nothing is kept.
        board.set_lane("ghost", Some("fast, over Wi-Fi"));
        assert!(board.lanes.is_empty());
        for n in 0..1000 {
            let id = format!("t-{n}");
            let device = format!("dev-{}", n % 7);
            let synced = n % 5 == 0;
            if synced {
                board.begin_synced(row(&id, &device, false, 9 << 20));
            } else {
                // A full board refuses a new row; then there is no lane either.
                let _ = board.begin(row(&id, &device, false, 9 << 20));
            }
            board.set_lane(&id, Some("fast, over Wi-Fi"));
            match n % 6 {
                0 => board.finish(&id, Outcome::Sent),
                1 => board.finish(&id, Outcome::Cancelled),
                2 => board.discard(&id),
                3 => board.pause_device(&device),
                // 4: left listed (cleaned with its device below).
                _ => {}
            }
            assert!(board.lanes.len() <= 2 * MAX_ROWS, "bounded while running");
        }
        // The paired device goes away (unpair, a dead peer): all its rows end.
        for d in 0..7 {
            board.finish_device(&format!("dev-{d}"), "gone");
        }
        assert!(board.rows().is_empty());
        assert_eq!(board.lanes.len(), 0, "{:?}", board.lanes);
    }

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

    /// S2: 500 files queued behind 20 live transfers never push a live one
    /// off the board; placeholders yield, and a full board refuses.
    #[test]
    fn a_long_queue_never_evicts_a_live_transfer() {
        let mut board = TransferBoard::default();
        for n in 0..20 {
            assert!(board.begin(row(&format!("live-{n}"), "phone", true, 10)));
        }
        for n in 0..500 {
            let mut placeholder = row(&format!("preparing-{n}"), "phone", false, 0);
            placeholder.preparing = true;
            board.begin(placeholder);
        }
        let ids: Vec<_> = board.rows().into_iter().map(|row| row.id).collect();
        assert!(ids.len() <= MAX_ROWS);
        for n in 0..20 {
            assert!(ids.contains(&format!("live-{n}")), "live-{n} was evicted");
        }
        // Real transfers still get in by taking a placeholder's place...
        assert!(board.begin(row("real", "phone", true, 10)));
        assert!(board.rows().iter().any(|row| row.id == "real"));
        // ...and with only live rows left the board says no.
        let mut full = TransferBoard::default();
        for n in 0..MAX_ROWS {
            assert!(full.begin(row(&format!("r{n}"), "phone", true, 10)));
        }
        assert!(!full.has_room());
        assert!(!full.begin(row("one-more", "phone", true, 10)));
        assert!(full.rows().iter().all(|row| row.id != "one-more"));
        assert!(full.rows().iter().any(|row| row.id == "r0"), "r0 kept");
    }

    /// I7: unpairing a phone ends its listed transfers (nothing called
    /// `finish_device` in production, so they stayed for ever).
    #[test]
    fn unpairing_a_phone_clears_its_transfer_rows() {
        super::begin(row("unpair-row", "phone-unpair-row", true, 10));
        assert!(super::rows().iter().any(|row| row.id == "unpair-row"));
        crate::device_links::peers::forget_peer("phone-unpair-row");
        assert!(!super::rows().iter().any(|row| row.id == "unpair-row"));
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
        let mut board = TransferBoard::default();
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

    /// A second reader follows the outcomes by number without draining them:
    /// the window's queue is untouched.
    #[test]
    fn the_outcomes_can_be_followed_without_taking_them() {
        let (_, mark) = ended_since(0);
        let id = format!("follow-{}", uuid::Uuid::new_v4());
        begin(TransferRow {
            id: id.clone(),
            device_id: "follow-phone".into(),
            name: "f.bin".into(),
            incoming: false,
            done: 0,
            total: 10,
            preparing: false,
            paused: false,
        });
        finish(&id, Outcome::Sent);
        let (news, next) = ended_since(mark);
        assert!(news
            .iter()
            .any(|f| f.id == id && f.outcome == Outcome::Sent));
        assert!(next > mark);
        let (again, _) = ended_since(next);
        assert!(again.iter().all(|f| f.id != id), "each is returned once");
        // The window's own queue still has it.
        assert!(peek_finished(&id).iter().any(|f| f.id == id));
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

    /// A Bucket row pushed off a full list is reported ended (its slot is
    /// freed) and failed (the file is asked for again), never just forgotten.
    #[test]
    fn a_synced_row_pushed_off_a_full_list_is_reported_ended_and_failed() {
        let mut board = TransferBoard::default();
        for n in 0..=MAX_ROWS {
            board.begin_synced(row(&format!("s{n}"), "phone", true, 9 << 20));
        }
        assert_eq!(board.synced.len(), MAX_ROWS);
        assert!(board
            .synced_ended
            .iter()
            .any(|end| end.name == "s0.bin" && !end.ok && end.incoming));
        assert_eq!(board.synced_failed.front().map(String::as_str), Some("s0"));
    }

    /// An unpair ends the phone's transfers: their lane tokens are void, and a
    /// failed row offers no "Try Again" for a phone that is gone.
    #[test]
    fn ending_a_devices_transfers_voids_their_tokens_and_their_retry() {
        let (id, device) = ("unpair-board-1", "unpair-board-dev");
        let token = super::super::file_lane::issue_token(id, device).unwrap();
        assert!(begin(row(id, device, false, 9 << 20)));
        set_retry_source(id, PathBuf::from("/tmp/x"));
        finish_device(device, "The phone was unpaired");
        assert!(!super::super::file_lane::spend_token_for_test(
            &token, id, device
        ));
        let ended = peek_finished(id);
        assert_eq!(ended.len(), 1, "{ended:?}");
        assert_eq!(ended[0].retry, None);
    }

    /// A row taken off the board without an outcome (a changed file begun
    /// again under a new id) takes its retry source with it.
    #[test]
    fn a_discarded_row_forgets_its_retry_source() {
        let mut board = TransferBoard::default();
        board.begin(row("r1", "phone", false, 5));
        board.set_retry_source("r1", PathBuf::from("/tmp/x"));
        board.discard("r1");
        assert!(board.retry_sources.is_empty());
    }
}
