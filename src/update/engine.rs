//! The checking engine: one place that turns "check now" into a verified,
//! stored answer and the state the rest of the app reads. It owns the state
//! file and a [`Fetch`]; the clock and the settings come in as arguments, so
//! every transition is testable without a network or a wait.
//!
//! Any error ends as a [`Status::Failed`] with a code and nothing else
//! changes: what was last verified stays what the person sees. A body that
//! does not verify is not stored and its ETag is not kept.
//!
//! Sibling edges: `model` (verify, decide), `fetch` (the seam), `store`,
//! `schedule`, `keys`.

use super::fetch::{Fetch, Fetched};
use super::model::{self, Channel, Decision, Key, Payload, Prefs, Version};
use super::schedule;
use super::store::Store;
use std::path::PathBuf;

/// What the app is doing about updates (the non-manifest half of 4.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Status {
    Idle,
    Checking,
    /// A code from [`model::Reject::code`] / [`super::fetch::FetchError::code`], and when
    /// the run of failures began (Unix seconds).
    Failed {
        reason: &'static str,
        since: u64,
    },
    /// "Check automatically" is off.
    Disabled,
    /// The build has no update address: the whole feature is inert.
    NotConfigured,
}

/// What the person's settings say, read once per cycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Settings {
    pub(crate) auto_check: bool,
    /// "beta", "stable" or "" (the line's default).
    pub(crate) channel: String,
}

pub(super) struct Engine<F: Fetch> {
    /// `None`: no usable address, nothing is ever fetched.
    fetcher: Option<F>,
    keys: Vec<Key>,
    installed: Version,
    path: Option<PathBuf>,
    pub(super) store: Store,
    payload: Option<Payload>,
    pub(super) status: Status,
    last_attempt: u64,
}

impl<F: Fetch> Engine<F> {
    pub(super) fn new(
        fetcher: Option<F>,
        keys: Vec<Key>,
        installed: Version,
        path: Option<PathBuf>,
        now: u64,
    ) -> Self {
        let store = path.as_deref().map(Store::load).unwrap_or_default();
        let mut engine = Engine {
            status: if fetcher.is_some() {
                Status::Idle
            } else {
                Status::NotConfigured
            },
            fetcher,
            keys,
            installed,
            path,
            store,
            payload: None,
            last_attempt: 0,
        };
        engine.reload_cache(now);
        engine
    }

    /// The cached manifest, checked again like a fresh one (a hand-edited
    /// file cannot forge it); one that no longer verifies is dropped.
    fn reload_cache(&mut self, now: u64) {
        self.payload = self.store.cached.clone().and_then(|text| {
            model::verify(text.as_bytes(), &self.keys, now, self.store.last_issued_at).ok()
        });
        if self.payload.is_none() {
            self.store.cached = None;
            self.store.etag = None;
        }
        if let Some(since) = self.store.failed_since {
            if let Some(reason) = self.store.fail_reason.as_deref().and_then(reason_code) {
                self.status = Status::Failed { reason, since };
            }
        }
        if self.fetcher.is_none() {
            self.status = Status::NotConfigured;
        }
    }

    fn save(&self) {
        if let Some(path) = &self.path {
            // Losing the file costs one extra check; never an error.
            let _ = self.store.save(path);
        }
    }

    pub(super) fn prefs(&self) -> Prefs {
        Prefs {
            skipped: self
                .store
                .skipped_version
                .as_deref()
                .and_then(Version::parse),
            snoozed: self
                .store
                .snoozed_version
                .as_deref()
                .and_then(Version::parse),
            snoozed_until: self.store.snoozed_until,
        }
    }

    /// The manifest-derived state now, from the cache.
    pub(super) fn decision(&self, settings: &Settings, now: u64, manual: bool) -> Decision {
        match &self.payload {
            Some(payload) if now <= payload.expires => model::decide(
                self.installed,
                Channel::chosen(&settings.channel, self.installed),
                payload,
                &self.prefs(),
                &self.store.install_id_bytes(),
                now,
                manual,
            ),
            _ => Decision::UpToDate,
        }
    }

    /// Seconds since the cache was last confirmed; `None` with no cache.
    pub(super) fn cache_age(&self, now: u64) -> Option<u64> {
        self.payload.as_ref()?;
        Some(now.saturating_sub(self.store.last_checked))
    }

    /// When the next automatic check is due (Unix seconds).
    pub(super) fn next_due(&self, seed: u64) -> u64 {
        self.store.last_checked.max(self.last_attempt)
            + schedule::next_delay(self.store.failures, seed)
    }

    /// One check. An automatic one does nothing while checks are off; a
    /// manual one always runs, but not twice within 5 seconds. Returns the
    /// status afterwards.
    pub(super) fn check(&mut self, settings: &Settings, now: u64, manual: bool) -> Status {
        if self.fetcher.is_none() {
            return self.status.clone();
        }
        if !manual && !settings.auto_check {
            self.status = Status::Disabled;
            return self.status.clone();
        }
        if manual
            && now.saturating_sub(self.last_attempt) < schedule::MANUAL_GAP
            && self.last_attempt != 0
        {
            return self.status.clone();
        }
        // An open or a timer must not turn an offline computer into a ten
        // second request each time: automatic checks keep a gap (a minute,
        // or the failure backoff of 1 h, 3 h, 6 h, 12 h, 24 h).
        if !manual && self.last_attempt != 0 {
            let gap = if self.store.failures > 0 {
                schedule::next_delay(self.store.failures, 0)
            } else {
                schedule::AUTO_GAP
            };
            if now.saturating_sub(self.last_attempt) < gap {
                return self.status.clone();
            }
        }
        self.last_attempt = now;
        self.status = Status::Checking;
        let etag = self.store.etag.clone().filter(|_| self.payload.is_some());
        let result = match self.fetcher.as_ref().map(|f| f.get(etag.as_deref())) {
            Some(result) => result,
            None => return self.status.clone(),
        };
        let outcome = match result {
            Ok(Fetched::NotModified) if self.payload.as_ref().is_some_and(|p| now <= p.expires) => {
                Ok(())
            }
            // A 304 confirms what is cached; an expired cache is not
            // confirmed by it (nothing would ever be offered from it), and
            // with no cache there is nothing to confirm: refetch next time.
            Ok(Fetched::NotModified) => {
                let expired = self.payload.is_some();
                self.store.etag = None;
                if expired {
                    self.payload = None;
                    self.store.cached = None;
                }
                Err(if expired { "expired" } else { "parse" })
            }
            Ok(Fetched::Body { bytes, etag }) => {
                match model::verify(&bytes, &self.keys, now, self.store.last_issued_at) {
                    Ok(payload) => {
                        // Never ahead of this clock, so one badly dated manifest cannot
                        // make every later (honest) one look like a replay.
                        self.store.last_issued_at = self
                            .store
                            .last_issued_at
                            .max(model::ratchet(payload.issued_at, now));
                        self.store.cached = String::from_utf8(bytes).ok();
                        self.store.etag = etag;
                        self.payload = Some(payload);
                        Ok(())
                    }
                    Err(reject) => Err(reject.code()),
                }
            }
            Err(error) => Err(error.code()),
        };
        match outcome {
            Ok(()) => {
                self.store.last_checked = now;
                self.store.failures = 0;
                self.store.failed_since = None;
                self.store.fail_reason = None;
                self.status = Status::Idle;
            }
            Err(reason) => {
                self.store.failures = self.store.failures.saturating_add(1);
                let since = *self.store.failed_since.get_or_insert(now);
                self.store.fail_reason = Some(reason.to_string());
                self.status = Status::Failed {
                    reason: reason_code(reason).unwrap_or("parse"),
                    since,
                };
            }
        }
        // What the person chose about a version that is no longer offered
        // (installed caught up, or a newer one replaced it) is dropped.
        self.tidy();
        self.save();
        self.status.clone()
    }

    /// Skip and snooze only mean something for the version they were made on.
    fn tidy(&mut self) {
        let offered = self.payload.as_ref().and_then(|p| {
            // Either channel's version may be the one the person was shown.
            [p.beta.as_ref(), p.stable.as_ref()]
                .into_iter()
                .flatten()
                .map(|c| c.version)
                .max()
        });
        let stale =
            |text: &Option<String>| match (text.as_deref().and_then(Version::parse), offered) {
                (Some(version), Some(offered)) => version < offered || version <= self.installed,
                (Some(version), None) => version <= self.installed,
                (None, _) => text.is_some(),
            };
        if stale(&self.store.skipped_version) {
            self.store.skipped_version = None;
        }
        if stale(&self.store.snoozed_version) {
            self.store.snoozed_version = None;
            self.store.snoozed_until = 0;
        }
    }

    pub(super) fn skip(&mut self, version: Version) {
        self.store.skipped_version = Some(version.to_string());
        self.save();
    }

    pub(super) fn unskip(&mut self) {
        self.store.skipped_version = None;
        self.save();
    }

    /// "Remind me tomorrow".
    pub(super) fn snooze(&mut self, version: Version, now: u64) {
        self.store.snoozed_version = Some(version.to_string());
        self.store.snoozed_until = now + schedule::DAY;
        self.save();
    }

    pub(super) fn installed(&self) -> Version {
        self.installed
    }

    #[cfg(test)]
    pub(super) fn set_installed(&mut self, version: Version) {
        self.installed = version;
        self.tidy();
    }
}

fn reason_code(text: &str) -> Option<&'static str> {
    [
        "offline",
        "timeout",
        "http",
        "oversized",
        "parse",
        "signature",
        "expired",
        "replay",
        "schema",
    ]
    .into_iter()
    .find(|code| *code == text)
}
