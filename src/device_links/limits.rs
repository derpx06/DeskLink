//! Shared guards for what a peer can make DeskLink do: rate-limited log
//! lines (a flood never floods the journal) and per-key token buckets.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// A repeated log line is written at most once per this long per kind.
const LOG_EVERY: Duration = Duration::from_secs(10);
/// Kinds of log line, and keys per bucket, remembered (then forgotten).
const MAX_KEYS: usize = 256;

/// Writes `line` unless a line of `kind` went out in the last
/// [`LOG_EVERY`]; the next one says how many were skipped. Never put
/// personal data (text, names, paths) in `line`.
pub(crate) fn log_limited(kind: &'static str, line: &str) {
    static LAST: Mutex<Option<HashMap<&'static str, (Instant, u64)>>> = Mutex::new(None);
    let Ok(mut last) = LAST.lock() else {
        return;
    };
    let last = last.get_or_insert_with(HashMap::new);
    if last.len() >= MAX_KEYS {
        last.clear();
    }
    match last.get_mut(kind) {
        Some((at, skipped)) if at.elapsed() < LOG_EVERY => *skipped += 1,
        Some((at, skipped)) => {
            eprintln!("[DeskLink] {line} ({skipped} similar skipped)");
            *at = Instant::now();
            *skipped = 0;
        }
        None => {
            eprintln!("[DeskLink] {line}");
            last.insert(kind, (Instant::now(), 0));
        }
    }
}

/// `max` per `per` for each key (a device id), refilled whole each period.
pub(crate) struct Buckets {
    max: u32,
    per: Duration,
    keys: Mutex<Option<HashMap<String, (Instant, u32)>>>,
}

impl Buckets {
    pub(crate) const fn new(max: u32, per: Duration) -> Self {
        Self {
            max,
            per,
            keys: Mutex::new(None),
        }
    }

    /// Takes one token for `key`; false when it has none left this period.
    pub(crate) fn allow(&self, key: &str) -> bool {
        let Ok(mut keys) = self.keys.lock() else {
            return false;
        };
        let keys = keys.get_or_insert_with(HashMap::new);
        if keys.len() >= MAX_KEYS && !keys.contains_key(key) {
            keys.retain(|_, (at, _)| at.elapsed() < self.per);
            if keys.len() >= MAX_KEYS {
                return false;
            }
        }
        let bucket = keys
            .entry(key.to_string())
            .or_insert((Instant::now(), self.max));
        if bucket.0.elapsed() >= self.per {
            *bucket = (Instant::now(), self.max);
        }
        if bucket.1 == 0 {
            return false;
        }
        bucket.1 -= 1;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bucket_allows_max_per_key_then_refuses() {
        let buckets = Buckets::new(3, Duration::from_secs(60));
        assert_eq!((0..5).filter(|_| buckets.allow("a")).count(), 3);
        assert!(buckets.allow("b"), "per key");
        let quick = Buckets::new(1, Duration::from_millis(1));
        assert!(quick.allow("a"));
        std::thread::sleep(Duration::from_millis(5));
        assert!(quick.allow("a"), "refilled after the period");
    }

    #[test]
    fn keys_are_bounded() {
        let buckets = Buckets::new(1, Duration::from_secs(60));
        for key in 0..MAX_KEYS {
            assert!(buckets.allow(&key.to_string()));
        }
        assert!(!buckets.allow("one too many"), "no room for a new key");
        log_limited("test", "a line");
        log_limited("test", "skipped");
    }
}
