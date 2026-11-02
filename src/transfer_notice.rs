//! One "DeskLink" notification per phone for the files moving between it and
//! this computer, in either direction (one-off sends and receives, and the
//! Bucket's sync): progress while it runs, an outcome when it ends.
//!
//! The state is the transfer board's (nothing new is counted here): the rows
//! that are listed now, and the outcomes it keeps. This module only decides
//! *what to say and when*, from a snapshot and a clock, so it can be tested
//! with a fake one; `application.rs` posts what it returns.
//!
//! - Nothing for a transfer that has not run for [`QUALIFIES_AFTER`].
//! - The progress notification is updated in place (one id per phone), at most
//!   every [`UPDATE_EVERY`] and only when its words changed, at low priority:
//!   it must never pop a banner again.
//! - When the last one ends it is replaced by the outcome, in plain words.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::bucket_progress::{about, size_words, speed_words, Rate};
use crate::device_links::webrtc::transfer_board::{Finished, Outcome, TransferRow};

/// A transfer shorter than this is not worth a notification.
pub(crate) const QUALIFIES_AFTER: Duration = Duration::from_secs(2);
/// The progress words change at most this often.
pub(crate) const UPDATE_EVERY: Duration = Duration::from_secs(5);

/// What to post.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Action {
    /// Create or update the progress notification of `device`.
    Progress { device: String, body: String },
    /// Replace it with how it ended. `path` is a received file (the
    /// notification then offers Open and Show in Files); `failed` is an
    /// error.
    Ended {
        device: String,
        title: String,
        path: Option<PathBuf>,
    },
    /// Take it down (cancelled by the person, or nothing worth saying).
    Clear { device: String },
}

struct Tracked {
    first_seen: Instant,
    rate: Rate,
    row: TransferRow,
    note: Option<String>,
}

#[derive(Default)]
struct Phone {
    rows: HashMap<String, Tracked>,
    /// A progress notification is up.
    shown: bool,
    last_update: Option<Instant>,
    last_body: String,
    /// Files finished in this batch, and how many were seen in all.
    batch_done: u32,
    batch_seen: u32,
    /// What ended in this batch that was worth a notification.
    outcomes: Vec<Finished>,
    /// A transfer of this batch ran long enough to be announced.
    qualified: bool,
}

#[derive(Default)]
pub(crate) struct Notices {
    phones: HashMap<String, Phone>,
    seen: u64,
}

fn per_second(seconds: u64) -> String {
    if seconds < 60 {
        let rounded = ((seconds + 2) / 5 * 5).max(5);
        format!("about {rounded} s left")
    } else {
        about(seconds)
    }
}

impl Notices {
    /// One look: the rows now (`rows`), the outcomes recorded since the last
    /// look (`ended`), the clock, how to name a phone and why a row waits.
    pub(crate) fn look(
        &mut self,
        now: Instant,
        rows: &[TransferRow],
        ended: &[Finished],
        name_of: &dyn Fn(&str) -> String,
        note_of: &dyn Fn(&str) -> Option<String>,
    ) -> Vec<Action> {
        let mut actions = Vec::new();
        // The rows that exist now.
        let mut present: HashMap<&str, ()> = HashMap::new();
        for row in rows.iter().filter(|row| !row.preparing) {
            present.insert(row.id.as_str(), ());
            let phone = self.phones.entry(row.device_id.clone()).or_default();
            let tracked = phone.rows.entry(row.id.clone()).or_insert_with(|| {
                phone.batch_seen += 1;
                Tracked {
                    first_seen: now,
                    rate: Rate::new(row.done, now),
                    row: row.clone(),
                    note: None,
                }
            });
            tracked.rate.sample(row.done, now);
            tracked.row = row.clone();
            tracked.note = note_of(&row.id);
        }
        // The rows that were here and are gone have ended.
        for (device, phone) in &mut self.phones {
            let gone: Vec<String> = phone
                .rows
                .keys()
                .filter(|id| !present.contains_key(id.as_str()))
                .cloned()
                .collect();
            for id in gone {
                let Some(tracked) = phone.rows.remove(&id) else {
                    continue;
                };
                let long = now.saturating_duration_since(tracked.first_seen) >= QUALIFIES_AFTER;
                phone.qualified |= long;
                phone.batch_done += 1;
                if let Some(finished) = ended.iter().find(|finished| finished.id == id) {
                    if long {
                        phone.outcomes.push(finished.clone());
                    }
                }
                let _ = device;
            }
        }
        // Say something per phone.
        let mut devices: Vec<String> = self.phones.keys().cloned().collect();
        devices.sort();
        for device in devices {
            let phone = self.phones.get_mut(&device).unwrap();
            let phone_name = name_of(&device);
            if phone.rows.is_empty() {
                if phone.qualified || phone.shown {
                    actions.extend(finish(&device, &phone_name, phone));
                }
                *phone = Phone::default();
                continue;
            }
            let long = phone
                .rows
                .values()
                .any(|t| now.saturating_duration_since(t.first_seen) >= QUALIFIES_AFTER);
            if !long {
                continue;
            }
            phone.qualified = true;
            let body = progress_body(&phone_name, phone, now);
            let due = phone
                .last_update
                .is_none_or(|at| now.saturating_duration_since(at) >= UPDATE_EVERY);
            if due && (!phone.shown || body != phone.last_body) {
                phone.shown = true;
                phone.last_update = Some(now);
                phone.last_body = body.clone();
                actions.push(Action::Progress {
                    device: device.clone(),
                    body,
                });
            }
        }
        self.phones
            .retain(|_, phone| !phone.rows.is_empty() || phone.shown);
        let _ = ended;
        actions
    }

    /// Numbers the outcomes already taken, so each is looked at once.
    pub(crate) fn seen(&self) -> u64 {
        self.seen
    }

    pub(crate) fn set_seen(&mut self, seen: u64) {
        self.seen = seen;
    }
}

/// The words of the progress notification.
fn progress_body(phone: &str, state: &Phone, now: Instant) -> String {
    let rows: Vec<&Tracked> = state.rows.values().collect();
    // Waiting for the phone, or for room on it: said as it is.
    if rows.iter().all(|tracked| tracked.row.paused) {
        let note = rows.iter().find_map(|tracked| tracked.note.clone());
        return note.unwrap_or_else(|| format!("Waiting for {phone}"));
    }
    let done: u64 = rows.iter().map(|t| t.row.done).sum();
    let total: u64 = rows.iter().map(|t| t.row.total).sum();
    let speed: f64 = rows
        .iter()
        .filter(|t| !t.row.paused)
        .filter_map(|t| t.rate.speed())
        .sum();
    let remaining = total.saturating_sub(done);
    let percent = (done as f64 * 100.0 / total.max(1) as f64).floor() as u64;
    let mut tail = vec![format!("{}%", percent.min(100))];
    if speed >= 1.0 {
        tail.push(speed_words(speed));
        let eta = rows
            .iter()
            .filter_map(|t| t.rate.eta(t.row.total.saturating_sub(t.row.done), now))
            .max();
        if let Some(eta) = eta.or_else(|| {
            let seconds = (remaining as f64 / speed).ceil();
            (seconds < 24.0 * 3600.0).then_some(seconds as u64)
        }) {
            tail.push(per_second(eta));
        }
    }
    let head = if rows.len() == 1 && state.batch_seen == 1 {
        let tracked = rows[0];
        let (verb, preposition) = if tracked.row.incoming {
            ("Receiving", "from")
        } else {
            ("Sending", "to")
        };
        format!(
            "{verb} {} {preposition} {phone} ({})",
            tracked.row.name,
            size_words(tracked.row.total)
        )
    } else {
        let all = state.batch_seen.max(rows.len() as u32 + state.batch_done);
        format!(
            "{all} files · {} of {all} · {phone}",
            (state.batch_done + 1).min(all)
        )
    };
    // The one-file form reads "Sending X to Y · 45% · 6.5 MiB/s · about 40 s left".
    let head = head.split(" (").next().unwrap_or(&head).to_string();
    format!("{head} · {}", tail.join(" · "))
}

/// How a batch ended, in plain words.
fn finish(device: &str, phone: &str, state: &mut Phone) -> Option<Action> {
    let outcomes = std::mem::take(&mut state.outcomes);
    let cancelled = outcomes
        .iter()
        .all(|finished| matches!(finished.outcome, Outcome::Cancelled));
    if outcomes.is_empty() {
        // Ended without an outcome of its own to read (a Bucket file, a
        // transfer that carried on under another id): nothing went wrong.
        return state.shown.then(|| Action::Ended {
            device: device.to_string(),
            title: format!("Up to date with {phone}"),
            path: None,
        });
    }
    if cancelled {
        return Some(Action::Clear {
            device: device.to_string(),
        });
    }
    if let Some(failed) = outcomes
        .iter()
        .find(|finished| matches!(finished.outcome, Outcome::Failed(_)))
    {
        let Outcome::Failed(why) = &failed.outcome else {
            return None;
        };
        let verb = if failed.incoming { "receive" } else { "send" };
        return Some(Action::Ended {
            device: device.to_string(),
            title: format!("Couldn't {verb} {}: {}", failed.name, plain(why)),
            path: None,
        });
    }
    let ok: Vec<&Finished> = outcomes
        .iter()
        .filter(|f| matches!(f.outcome, Outcome::Sent | Outcome::Received(_)))
        .collect();
    let (incoming, outgoing) = (
        ok.iter().filter(|f| f.incoming).count(),
        ok.iter().filter(|f| !f.incoming).count(),
    );
    let title = match (ok.len(), incoming, outgoing) {
        (1, 1, _) => format!("Received {} from {phone}", ok[0].name),
        (1, _, 1) => format!("Sent {} to {phone}", ok[0].name),
        (n, i, 0) if i == n => format!("Received {n} files from {phone}"),
        (n, 0, o) if o == n => format!("Sent {n} files to {phone}"),
        (n, _, _) => format!("Finished {n} transfers with {phone}"),
    };
    let path = (ok.len() == 1)
        .then(|| match &ok[0].outcome {
            Outcome::Received(path) => Some(path.clone()),
            _ => None,
        })
        .flatten();
    Some(Action::Ended {
        device: device.to_string(),
        title,
        path,
    })
}

/// A reason, one line, no stop at the end that a title would double.
fn plain(reason: &str) -> String {
    reason
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .trim_end_matches('.')
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(
        id: &str,
        device: &str,
        name: &str,
        incoming: bool,
        done: u64,
        total: u64,
    ) -> TransferRow {
        TransferRow {
            id: id.into(),
            device_id: device.into(),
            name: name.into(),
            incoming,
            done,
            total,
            preparing: false,
            paused: false,
        }
    }

    fn finished(id: &str, device: &str, name: &str, incoming: bool, outcome: Outcome) -> Finished {
        Finished {
            id: id.into(),
            device_id: device.into(),
            name: name.into(),
            incoming,
            outcome,
            retry: None,
        }
    }

    fn name(_: &str) -> String {
        "Pixel".to_string()
    }

    fn none(_: &str) -> Option<String> {
        None
    }

    const MB: u64 = 1_000_000;

    fn at(start: Instant, seconds: f64) -> Instant {
        start + Duration::from_secs_f64(seconds)
    }

    /// Nothing for a transfer under two seconds; then one "Sending X to Y"
    /// with percent, speed and time left; nothing again until five seconds on.
    #[test]
    fn a_long_send_gets_one_notification_updated_in_place_and_not_too_often() {
        let (mut notices, t0) = (Notices::default(), Instant::now());
        let mut look = |s: f64, done: u64| {
            notices.look(
                at(t0, s),
                &[row("t", "p", "big.bin", false, done, 300 * MB)],
                &[],
                &name,
                &none,
            )
        };
        assert!(look(0.0, 0).is_empty());
        assert!(look(1.0, 7 * MB).is_empty(), "under two seconds");
        let shown = look(2.5, 16 * MB);
        assert_eq!(shown.len(), 1);
        let Action::Progress { device, body } = &shown[0] else {
            panic!("{shown:?}")
        };
        assert_eq!(device, "p");
        assert!(
            body.starts_with("Sending big.bin to Pixel · 5% · "),
            "{body}"
        );
        assert!(body.contains("/s · about "), "{body}");
        assert!(body.ends_with(" left"), "{body}");
        // The body changes every look, but it is said every five seconds.
        assert!(look(4.0, 30 * MB).is_empty());
        assert!(look(7.4, 60 * MB).is_empty());
        let next = look(7.6, 62 * MB);
        assert_eq!(next.len(), 1, "five seconds after the last");
    }

    /// The send ends: the progress is replaced by what happened, in words.
    #[test]
    fn a_finished_transfer_is_replaced_by_how_it_ended() {
        let (mut notices, t0) = (Notices::default(), Instant::now());
        let r = |done| [row("t", "p", "a.bin", false, done, 10 * MB)];
        notices.look(at(t0, 0.0), &r(0), &[], &name, &none);
        notices.look(at(t0, 3.0), &r(5 * MB), &[], &name, &none);
        let ended = [finished("t", "p", "a.bin", false, Outcome::Sent)];
        let actions = notices.look(at(t0, 4.0), &[], &ended, &name, &none);
        assert_eq!(
            actions,
            [Action::Ended {
                device: "p".into(),
                title: "Sent a.bin to Pixel".into(),
                path: None
            }]
        );
        // A received file offers Open and Show in Files (it has a path).
        let (mut notices, t0) = (Notices::default(), Instant::now());
        let r = |done| [row("t", "p", "a.bin", true, done, 10 * MB)];
        notices.look(at(t0, 0.0), &r(0), &[], &name, &none);
        notices.look(at(t0, 3.0), &r(5 * MB), &[], &name, &none);
        let ended = [finished(
            "t",
            "p",
            "a.bin",
            true,
            Outcome::Received("/home/me/Downloads/a.bin".into()),
        )];
        let actions = notices.look(at(t0, 4.0), &[], &ended, &name, &none);
        assert_eq!(
            actions,
            [Action::Ended {
                device: "p".into(),
                title: "Received a.bin from Pixel".into(),
                path: Some("/home/me/Downloads/a.bin".into())
            }]
        );
        // And nothing is left to say afterwards.
        assert!(notices.look(at(t0, 5.0), &[], &[], &name, &none).is_empty());
    }

    #[test]
    fn a_short_transfer_never_notifies() {
        let (mut notices, t0) = (Notices::default(), Instant::now());
        let r = [row("t", "p", "tiny.txt", false, 1, 10)];
        assert!(notices.look(at(t0, 0.0), &r, &[], &name, &none).is_empty());
        let ended = [finished("t", "p", "tiny.txt", false, Outcome::Sent)];
        assert!(notices
            .look(at(t0, 1.0), &[], &ended, &name, &none)
            .is_empty());
    }

    #[test]
    fn a_failure_is_said_plainly_and_a_cancel_just_takes_it_down() {
        let (mut notices, t0) = (Notices::default(), Instant::now());
        let r = [row("t", "p", "a.bin", false, 5 * MB, 10 * MB)];
        notices.look(at(t0, 0.0), &r, &[], &name, &none);
        notices.look(at(t0, 3.0), &r, &[], &name, &none);
        let failed = [finished(
            "t",
            "p",
            "a.bin",
            false,
            Outcome::Failed("Pixel's storage is full.".into()),
        )];
        assert_eq!(
            notices.look(at(t0, 4.0), &[], &failed, &name, &none),
            [Action::Ended {
                device: "p".into(),
                title: "Couldn't send a.bin: Pixel's storage is full".into(),
                path: None
            }]
        );
        let (mut notices, t0) = (Notices::default(), Instant::now());
        notices.look(at(t0, 0.0), &r, &[], &name, &none);
        notices.look(at(t0, 3.0), &r, &[], &name, &none);
        let cancelled = [finished("t", "p", "a.bin", false, Outcome::Cancelled)];
        assert_eq!(
            notices.look(at(t0, 4.0), &[], &cancelled, &name, &none),
            [Action::Clear { device: "p".into() }]
        );
    }

    /// A phone that dropped, or has no room, is said in words.
    #[test]
    fn a_paused_transfer_says_why() {
        let (mut notices, t0) = (Notices::default(), Instant::now());
        let mut paused = row("t", "p", "a.bin", false, 5 * MB, 10 * MB);
        paused.paused = true;
        notices.look(at(t0, 0.0), &[paused.clone()], &[], &name, &none);
        let actions = notices.look(at(t0, 3.0), &[paused.clone()], &[], &name, &none);
        assert_eq!(
            actions,
            [Action::Progress {
                device: "p".into(),
                body: "Waiting for Pixel".into()
            }]
        );
        let note = |_: &str| Some("Phone storage is full".to_string());
        let actions = notices.look(at(t0, 9.0), &[paused], &[], &name, &note);
        assert_eq!(
            actions,
            [Action::Progress {
                device: "p".into(),
                body: "Phone storage is full".into()
            }]
        );
    }

    /// Several files at once: "N files · k of N", one notification for the
    /// phone; another phone has its own.
    #[test]
    fn several_files_share_one_notification_and_each_phone_has_its_own() {
        let (mut notices, t0) = (Notices::default(), Instant::now());
        let rows = |d1, d2| {
            vec![
                row("a", "p", "a.bin", false, d1, 10 * MB),
                row("b", "p", "b.bin", false, d2, 10 * MB),
                row("c", "q", "c.bin", true, d1, 10 * MB),
            ]
        };
        notices.look(at(t0, 0.0), &rows(0, 0), &[], &name, &none);
        let actions = notices.look(at(t0, 3.0), &rows(MB, MB), &[], &name, &none);
        assert_eq!(actions.len(), 2, "one per phone: {actions:?}");
        let body_of = |device: &str| {
            actions
                .iter()
                .find_map(|a| match a {
                    Action::Progress { device: d, body } if d == device => Some(body.clone()),
                    _ => None,
                })
                .unwrap()
        };
        assert!(
            body_of("p").starts_with("2 files · 1 of 2 · Pixel · "),
            "{}",
            body_of("p")
        );
        assert!(
            body_of("q").starts_with("Receiving c.bin from Pixel · "),
            "{}",
            body_of("q")
        );
        // One finishes: "2 of 2" next time it is said.
        let only_b = vec![row("b", "p", "b.bin", false, 5 * MB, 10 * MB)];
        let ended = [finished("a", "p", "a.bin", false, Outcome::Sent)];
        let actions = notices.look(at(t0, 9.0), &only_b, &ended, &name, &none);
        let body = actions
            .iter()
            .find_map(|a| match a {
                Action::Progress { device, body } if device == "p" => Some(body),
                _ => None,
            })
            .unwrap_or_else(|| panic!("{actions:?}"));
        assert!(body.starts_with("2 files · 2 of 2 · Pixel · "), "{body}");
    }
}
