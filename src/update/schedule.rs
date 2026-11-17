//! When the next check happens: the daily refresh with jitter and the
//! backoff after failures (pure functions; the caller owns the clock).
//!
//! Sibling edges: none. Outside: std only.

pub(super) const DAY: u64 = 86_400;
/// A cache older than this is refreshed at the next open.
pub(super) const STALE_AFTER: u64 = DAY;
/// Between two manual checks.
pub(super) const MANUAL_GAP: u64 = 5;
/// Between two automatic checks after a success (an open must not refetch).
pub(super) const AUTO_GAP: u64 = 60;
/// How long the update thread sleeps when there is nothing to schedule
/// (checks off): only a command wakes it earlier.
pub(super) const IDLE_WAIT: u64 = 365 * DAY;
/// After a failed automatic check: 1 h, 3 h, 6 h, 12 h, then 24 h.
const BACKOFF: [u64; 5] = [3_600, 3 * 3_600, 6 * 3_600, 12 * 3_600, DAY];

/// Seconds until the next automatic check. `failures` is the number of
/// failed checks in a row (0 after a success); `seed` is any random number
/// and spreads a successful day by plus or minus 10 %.
pub(super) fn next_delay(failures: u32, seed: u64) -> u64 {
    if failures == 0 {
        let span = DAY / 5;
        return DAY - DAY / 10 + seed % (span + 1);
    }
    BACKOFF[(failures as usize - 1).min(BACKOFF.len() - 1)]
}
