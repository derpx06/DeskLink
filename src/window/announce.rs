//! Which of the phones' notifications are announced here (a toast, or the
//! desktop's own notification), and why each one is or is not.
//!
//! A pure decision over the devices' lists: the window only displays what it
//! returns. Every verdict goes to the `DESKLINK_DEBUG` log (key, signature,
//! verdict, reason; never the text) so a "twice" report can be read back.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::time::{Duration, Instant};

use crate::device_links::device::{DeviceNotification, DeviceView};

/// A conversation is announced again at most this often, and a burst at most
/// [`PER_PASS`] at a time (a group chat of thirty messages is not thirty banners).
const AGAIN_AFTER: Duration = Duration::from_secs(4);
const PER_PASS: u32 = 3;
/// A notification whose words change only in their digits ("Downloading 41%",
/// "3 new messages") is announced again at most this often.
const COUNTER_AGAIN_AFTER: Duration = Duration::from_secs(60);
/// One removed and posted again with the same words within this time is the
/// app re-posting (cancel and notify), not news.
const REPOST_QUIET: Duration = Duration::from_secs(10);
/// The same words from the same app under two ids within this time are one
/// notification (an app posting both a child and a copy).
const TWIN_QUIET: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy)]
struct Seen {
    signature: u64,
    /// The words without their digits.
    shape: u64,
    at: Option<Instant>,
}

/// The same conversation notification, a new message: the id stays, the text changes.
pub(super) fn signature(title: &str, text: &str, ticker: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    (title, text, ticker).hash(&mut hasher);
    hasher.finish()
}

fn shape(notification: &DeviceNotification) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for part in [
        &notification.title,
        &notification.text,
        &notification.ticker,
    ] {
        part.chars()
            .filter(|c| !c.is_ascii_digit())
            .for_each(|c| c.hash(&mut hasher));
        '|'.hash(&mut hasher);
    }
    hasher.finish()
}

fn content(notification: &DeviceNotification) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    (
        &notification.app_name,
        &notification.title,
        &notification.text,
        &notification.ticker,
    )
        .hash(&mut hasher);
    hasher.finish()
}

#[derive(Debug, Default)]
pub struct Tracker {
    seen: HashMap<String, Seen>,
    /// Ids that left the list: signature and when.
    gone: HashMap<String, (u64, Instant)>,
    /// (device, words) announced lately.
    twins: HashMap<(String, u64), Instant>,
}

impl Tracker {
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// Marks `notification` as already seen (no announcement later).
    #[cfg(debug_assertions)]
    pub fn mark_seen(&mut self, unique_id: String, notification: &DeviceNotification) {
        let signature = signature(
            &notification.title,
            &notification.text,
            &notification.ticker,
        );
        let shape = shape(notification);
        self.seen.insert(
            unique_id,
            Seen {
                signature,
                shape,
                at: None,
            },
        );
    }

    /// The notifications to announce now. `shows(device)`: its notifications
    /// are mirrored here; `extension`: the top-bar extension shows them.
    pub fn pass<'a>(
        &mut self,
        devices: &'a [DeviceView],
        shows: &dyn Fn(&str) -> bool,
        extension: bool,
        now: Instant,
    ) -> Vec<(&'a DeviceView, &'a DeviceNotification)> {
        let present: std::collections::HashSet<String> = devices
            .iter()
            .flat_map(|d| d.notifications.iter().map(|n| format!("{}:{}", d.id, n.id)))
            .collect();
        let before: Vec<String> = self.seen.keys().cloned().collect();
        for id in before {
            if !present.contains(&id) {
                if let Some(seen) = self.seen.remove(&id) {
                    self.gone.insert(id, (seen.signature, now));
                }
            }
        }
        self.gone
            .retain(|_, (_, at)| now.saturating_duration_since(*at) < REPOST_QUIET);
        self.twins
            .retain(|_, at| now.saturating_duration_since(*at) < TWIN_QUIET);

        let mut out = Vec::new();
        let mut announced = 0;
        for device in devices {
            for notification in &device.notifications {
                let unique_id = format!("{}:{}", device.id, notification.id);
                let sig = signature(
                    &notification.title,
                    &notification.text,
                    &notification.ticker,
                );
                let before = self.seen.get(&unique_id).copied();
                let shape = shape(notification);
                let twin_key = (device.id.clone(), content(notification));
                let verdict = if !device.trusted || !device.feature_transport_ready {
                    Err("device not live")
                } else if announced >= PER_PASS {
                    Err("burst cap")
                } else {
                    match before {
                        Some(known) if known.signature == sig => Err("unchanged"),
                        Some(known)
                            if known.at.is_some_and(|at| {
                                now.saturating_duration_since(at) < AGAIN_AFTER
                            }) =>
                        {
                            Err("changed too soon")
                        }
                        Some(known)
                            if known.shape == shape
                                && known.at.is_some_and(|at| {
                                    now.saturating_duration_since(at) < COUNTER_AGAIN_AFTER
                                }) =>
                        {
                            Err("a counter ticking")
                        }
                        Some(_) => Ok("changed"),
                        None if self
                            .gone
                            .get(&unique_id)
                            .is_some_and(|(gone_sig, _)| *gone_sig == sig) =>
                        {
                            Err("re-post of a removed one")
                        }
                        None => Ok("new"),
                    }
                }
                .and_then(|reason| {
                    if self.twins.contains_key(&twin_key) {
                        Err("same words under another id")
                    } else if !shows(&device.id) {
                        Err("mirroring off")
                    } else if notification.silent {
                        Err("resync (silent)")
                    } else if extension {
                        Err("the extension shows it")
                    } else {
                        Ok(reason)
                    }
                });
                // Remembered whatever the verdict: turning mirroring back on replays nothing.
                let announce = verdict.is_ok();
                self.seen.insert(
                    unique_id.clone(),
                    Seen {
                        signature: sig,
                        shape,
                        at: if announce {
                            Some(now)
                        } else {
                            before.and_then(|b| b.at)
                        },
                    },
                );
                debug_log!(
                    "[Notifications] announce {unique_id} sig={sig:016x}: {} ({})",
                    if announce { "shown" } else { "skipped" },
                    match verdict {
                        Ok(reason) | Err(reason) => reason,
                    }
                );
                if announce {
                    announced += 1;
                    self.twins.insert(twin_key, now);
                    out.push((device, notification));
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(texts: &[(&str, &str, bool)]) -> DeviceView {
        let info = crate::device_links::device_info::DeviceInfo::local(
            "0123456789abcdef0123456789abcdef".to_string(),
            "Pixel".to_string(),
        );
        let mut view = DeviceView::from_info(&info, String::new(), true);
        view.trusted = true;
        view.feature_transport_ready = true;
        view.notifications = texts
            .iter()
            .map(|(id, text, silent)| DeviceNotification {
                id: id.to_string(),
                app_name: "Chat".into(),
                title: "Mara".into(),
                text: text.to_string(),
                ticker: String::new(),
                is_clearable: true,
                request_reply_id: None,
                actions: Vec::new(),
                silent: *silent,
                time: 0,
            })
            .collect();
        view
    }

    /// Mirroring off, or the extension showing them, announces nothing, and
    /// what was marked then is not replayed when it changes back.
    #[test]
    fn hidden_and_extension_shown_ones_are_marked_not_replayed() {
        let t0 = Instant::now();
        let views = [view(&[("a", "hi", false)])];
        let mut tracker = Tracker::default();
        assert!(tracker.pass(&views, &|_| false, false, t0).is_empty());
        assert!(tracker.pass(&views, &|_| true, false, t0).is_empty());
        let mut tracker = Tracker::default();
        assert!(tracker.pass(&views, &|_| true, true, t0).is_empty());
        assert!(tracker.pass(&views, &|_| true, false, t0).is_empty());
    }

    /// A burst is at most three, the rest are not queued for later.
    #[test]
    fn a_burst_is_three_not_thirty() {
        let t0 = Instant::now();
        let all: Vec<(String, String, bool)> = (0..10)
            .map(|n| (format!("n{n}"), format!("message {n}"), false))
            .collect();
        let refs: Vec<(&str, &str, bool)> = all
            .iter()
            .map(|(a, b, c)| (a.as_str(), b.as_str(), *c))
            .collect();
        let views = [view(&refs)];
        let mut tracker = Tracker::default();
        assert_eq!(tracker.pass(&views, &|_| true, false, t0).len(), 3);
        assert!(tracker.pass(&views, &|_| true, false, t0).is_empty());
    }
}
