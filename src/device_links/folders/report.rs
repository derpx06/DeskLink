//! What an add, move or new folder left out ([`Report`]) and the queue of those
//! messages for the window ([`take_report`]).
//!
//! Sibling edges: none (the parent's limits only).

use super::*;

/// What an add, move or new folder left out, in plain words (the counts are
/// what the user is told).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct Report {
    pub(crate) added: usize,
    pub(crate) too_deep: usize,
    pub(crate) too_big: usize,
    pub(crate) bad_names: usize,
    pub(crate) over_items: usize,
    pub(crate) over_bytes: usize,
    /// The Bucket would pass 10 GiB.
    pub(crate) full: bool,
    pub(crate) not_moved: usize,
    /// Hidden, `.git`, `*.tmp`... items the Bucket never syncs: left out at
    /// add time rather than copied and then never sent.
    pub(crate) hidden: usize,
    /// Copies that failed (unreadable, disk full, vanished).
    pub(crate) failed: usize,
    /// Files removed on this side that came back because the other side
    /// changed them (a few names).
    pub(crate) restored: Vec<String>,
    pub(crate) refused: Option<String>,
}

impl Report {
    /// The one message for the user, if there is anything to say.
    pub(crate) fn message(&self) -> Option<String> {
        let mut parts = Vec::new();
        if let Some(refused) = &self.refused {
            parts.push(refused.clone());
        }
        let counted = |what: &str, count: usize| match count {
            1 => what.to_string(),
            count => format!("{what} ({count})"),
        };
        if self.too_big > 0 {
            parts.push(counted(
                "Files over 1 GiB can't go in the Bucket",
                self.too_big,
            ));
        }
        if self.too_deep > 0 {
            parts.push(format!(
                "Some items were more than {MAX_BUCKET_DEPTH} folders deep and were left out ({})",
                self.too_deep
            ));
        }
        if self.bad_names > 0 {
            parts.push(format!(
                "Some names can't sync, so they were left out ({})",
                self.bad_names
            ));
        }
        if self.full {
            parts.push("The Bucket is full (10 GiB)".to_string());
        }
        if self.over_items > 0 {
            parts.push(format!(
                "Only {MAX_ADD_ITEMS} items can be added at once; {} were left out",
                self.over_items
            ));
        }
        if self.over_bytes > 0 {
            parts.push(format!(
                "Only 5 GiB can be added at once; {} were left out",
                self.over_bytes
            ));
        }
        if let Some(first) = self.restored.first() {
            parts.push(match self.restored.len() {
                1 => format!("{first} was restored: it was changed on the other side"),
                count => format!(
                    "{first} and {} more were restored: they were changed on the other side",
                    count - 1
                ),
            });
        }
        if self.hidden > 0 {
            parts.push(format!(
                "Hidden and temporary items don't sync, so they were left out ({})",
                self.hidden
            ));
        }
        if self.failed > 0 {
            parts.push(format!(
                "{} couldn't be copied",
                match self.failed {
                    1 => "1 item".to_string(),
                    count => format!("{count} items"),
                }
            ));
        }
        if self.not_moved > 0 {
            parts.push(format!(
                "{} couldn't be moved: they would be more than {MAX_BUCKET_DEPTH} folders deep",
                match self.not_moved {
                    1 => "1 item".to_string(),
                    count => format!("{count} items"),
                }
            ));
        }
        if self.added > 0 {
            parts.insert(
                0,
                match self.added {
                    1 => "Added 1 item to the Bucket".to_string(),
                    count => format!("Added {count} items to the Bucket"),
                },
            );
        }
        (!parts.is_empty()).then(|| parts.join(". "))
    }
}

/// Messages for the user, waiting for the window (or a notification).
/// (Tests keep one queue per test thread: they all run in one process and
/// used to take each other's messages.)
#[cfg(not(test))]
static REPORTS: Mutex<VecDeque<String>> = Mutex::new(VecDeque::new());
#[cfg(test)]
thread_local! {
    static REPORTS_LOCAL: std::cell::RefCell<VecDeque<String>> = const { std::cell::RefCell::new(VecDeque::new()) };
}

pub(super) fn with_reports<T>(job: impl FnOnce(&mut VecDeque<String>) -> T) -> Option<T> {
    #[cfg(test)]
    {
        Some(REPORTS_LOCAL.with(|reports| job(&mut reports.borrow_mut())))
    }
    #[cfg(not(test))]
    {
        REPORTS.lock().ok().map(|mut reports| job(&mut reports))
    }
}

pub(super) fn tell(report: &Report) {
    if let Some(message) = report.message() {
        with_reports(|reports| {
            if reports.len() >= 8 {
                reports.pop_front();
            }
            reports.push_back(message);
        });
    }
}

/// The next message about what the Bucket left out, if any.
pub(crate) fn take_report() -> Option<String> {
    with_reports(|reports| reports.pop_front()).flatten()
}
