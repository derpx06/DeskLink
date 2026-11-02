//! Live activities (W13): the phone's ongoing items (a timer, a download, a
//! navigation) as `desklink.activity` snapshots, shown on the Overview and in
//! the top bar. Phone to desk only; kept in memory; contract in
//! `docs/protocol/stageD-protocol.md`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Items kept from one snapshot.
pub(crate) const MAX_ITEMS: usize = 5;
/// A countdown that ended this long ago is no longer news.
const ENDED_VISIBLE_MS: i64 = 60_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Activity {
    pub id: String,
    pub app_label: String,
    pub title: String,
    pub text: String,
    /// 0..=100, or -1 for none.
    pub progress: i32,
    /// Milliseconds on the phone's clock; -1 for none.
    pub chronometer_base: i64,
    /// `chronometer_base` is the end (true) or the start (false).
    pub countdown: bool,
    /// timer, progress, navigation, call or other.
    pub kind: String,
}

/// The latest snapshot of a phone and how its clock related to ours.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivitySnapshot {
    pub items: Vec<Activity>,
    /// This computer's clock minus the phone's, in ms, when it arrived.
    pub clock_offset_ms: i64,
}

fn clean(value: &str, max_chars: usize) -> String {
    value
        .chars()
        .filter(|c| !c.is_control())
        .take(max_chars)
        .collect()
}

/// The items a snapshot body holds, as the desk keeps them.
pub(crate) fn parse_items(body: &serde_json::Map<String, Value>) -> Vec<Activity> {
    let mut kept: Vec<Activity> = Vec::new();
    let Some(Value::Array(items)) = body.get("items") else {
        return kept;
    };
    for item in items {
        if kept.len() == MAX_ITEMS {
            break;
        }
        let Some(item) = item.as_object() else {
            continue;
        };
        let text =
            |key: &str, max: usize| clean(item.get(key).and_then(Value::as_str).unwrap_or(""), max);
        let id = text("id", 240);
        if id.is_empty() || kept.iter().any(|known| known.id == id) {
            continue;
        }
        let progress = match item.get("progress").and_then(Value::as_i64) {
            Some(percent) if percent >= 0 => percent.min(100) as i32,
            _ => -1,
        };
        let base = item
            .get("chronometerBase")
            .and_then(Value::as_i64)
            .filter(|base| *base > 0)
            .unwrap_or(-1);
        let kind = match item.get("kind").and_then(Value::as_str) {
            Some(kind @ ("timer" | "progress" | "navigation" | "call")) => kind.to_string(),
            _ => "other".to_string(),
        };
        kept.push(Activity {
            id,
            app_label: text("appLabel", 120),
            title: text("title", 240),
            text: text("text", 500),
            progress,
            chronometer_base: base,
            countdown: item
                .get("countdown")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            kind,
        });
    }
    kept
}

/// The snapshot a packet body makes, given this computer's clock `now_ms`.
pub(crate) fn snapshot_from(
    body: &serde_json::Map<String, Value>,
    now_ms: i64,
) -> ActivitySnapshot {
    let at = body.get("at").and_then(Value::as_i64).filter(|at| *at > 0);
    ActivitySnapshot {
        items: parse_items(body),
        clock_offset_ms: at.map_or(0, |at| now_ms - at),
    }
}

impl Activity {
    /// The phone's clock now, as far as this computer can tell.
    fn phone_now(snapshot: &ActivitySnapshot, now_ms: i64) -> i64 {
        now_ms - snapshot.clock_offset_ms
    }

    /// Whether to show it at `now_ms`: a countdown that ended long ago is not.
    pub fn visible(&self, snapshot: &ActivitySnapshot, now_ms: i64) -> bool {
        !(self.countdown
            && self.chronometer_base > 0
            && Self::phone_now(snapshot, now_ms) - self.chronometer_base > ENDED_VISIBLE_MS)
    }

    /// The live part, in words: "4:32 left", "0:12 elapsed", "42%", or none.
    pub fn live_text(&self, snapshot: &ActivitySnapshot, now_ms: i64) -> Option<String> {
        if self.chronometer_base > 0 {
            let phone_now = Self::phone_now(snapshot, now_ms);
            return Some(if self.countdown {
                let left = self.chronometer_base - phone_now;
                if left <= 0 {
                    "Done".to_string()
                } else {
                    format!("{} left", clock(left))
                }
            } else {
                format!(
                    "{} elapsed",
                    clock((phone_now - self.chronometer_base).max(0))
                )
            });
        }
        (self.progress >= 0).then(|| format!("{}%", self.progress))
    }
}

/// "m:ss", or "h:mm:ss" from an hour (rounded up to the next second for a
/// countdown's sake: 0.4 s left reads 0:01).
fn clock(ms: i64) -> String {
    let seconds = (ms.max(0) + 999) / 1000;
    let (h, m, s) = (seconds / 3600, seconds / 60 % 60, seconds % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// What to show for a phone: its visible items at `now_ms`, when it is ready
/// and the person has not hidden its notifications.
pub fn shown<'a>(
    snapshot: Option<&'a ActivitySnapshot>,
    ready: bool,
    notifications_shown: bool,
    now_ms: i64,
) -> Vec<&'a Activity> {
    match snapshot {
        Some(snapshot) if ready && notifications_shown => snapshot
            .items
            .iter()
            .filter(|item| item.visible(snapshot, now_ms))
            .collect(),
        _ => Vec::new(),
    }
}

/// The milliseconds since the epoch on this computer's clock.
pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(items: Value) -> serde_json::Map<String, Value> {
        let mut map = serde_json::Map::new();
        map.insert("items".into(), items);
        map
    }

    fn item(id: &str, kind: &str) -> Value {
        serde_json::json!({"id": id, "appLabel": "Clock", "title": "Pasta", "text": "",
            "progress": -1, "chronometerBase": -1, "countdown": false, "kind": kind})
    }

    /// The shared vectors (`activity_bodies.json`), read with this code.
    #[test]
    fn the_shared_bodies_keep_what_the_contract_says() {
        let f: Value =
            serde_json::from_str(include_str!("../../testdata/activity_bodies.json")).unwrap();
        for case in f["cases"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let kept = parse_items(&body(case["items"].clone()));
            let expect: Vec<Activity> = case["expect"]
                .as_array()
                .unwrap()
                .iter()
                .map(|e| Activity {
                    id: e["id"].as_str().unwrap().into(),
                    app_label: e["appLabel"].as_str().unwrap().into(),
                    title: e["title"].as_str().unwrap().into(),
                    text: e["text"].as_str().unwrap().into(),
                    progress: e["progress"].as_i64().unwrap() as i32,
                    chronometer_base: e["chronometerBase"].as_i64().unwrap(),
                    countdown: e["countdown"].as_bool().unwrap(),
                    kind: e["kind"].as_str().unwrap().into(),
                })
                .collect();
            assert_eq!(kept, expect, "{name}");
        }
    }

    #[test]
    fn long_strings_are_cut_and_control_characters_dropped() {
        let long = "a".repeat(600);
        let mut it = item("x", "timer");
        it["title"] = Value::String(long.clone());
        it["text"] = Value::String(long.clone());
        it["appLabel"] = Value::String(format!("Cl\u{7}ock{long}"));
        let kept = parse_items(&body(Value::Array(vec![it])));
        assert_eq!(kept[0].title.chars().count(), 240);
        assert_eq!(kept[0].text.chars().count(), 500);
        assert_eq!(kept[0].app_label.chars().count(), 120);
        assert!(kept[0].app_label.starts_with("Clock"));
        assert!(
            parse_items(&serde_json::Map::new()).is_empty(),
            "no items key"
        );
    }

    /// With a fake clock: a phone clock an hour off changes nothing, the
    /// countdown runs down, a count-up runs up, and an old end is hidden.
    #[test]
    fn a_countdown_runs_on_the_phones_clock_whatever_this_one_says() {
        let phone_at = 1_000_000_000_000_i64;
        let desk_at = phone_at + 3_600_000; // this clock is an hour ahead
        let mut it = item("t", "timer");
        it["chronometerBase"] = Value::from(phone_at + 272_000);
        it["countdown"] = Value::Bool(true);
        let mut b = body(Value::Array(vec![it]));
        b.insert("at".into(), Value::from(phone_at));
        let snapshot = snapshot_from(&b, desk_at);
        assert_eq!(snapshot.clock_offset_ms, 3_600_000);
        let t = &snapshot.items[0];
        assert_eq!(
            t.live_text(&snapshot, desk_at).as_deref(),
            Some("4:32 left")
        );
        assert_eq!(
            t.live_text(&snapshot, desk_at + 12_000).as_deref(),
            Some("4:20 left")
        );
        assert_eq!(
            t.live_text(&snapshot, desk_at + 272_000).as_deref(),
            Some("Done")
        );
        assert!(t.visible(&snapshot, desk_at + 272_000 + 60_000));
        assert!(
            !t.visible(&snapshot, desk_at + 272_000 + 60_001),
            "old news"
        );

        let mut up = item("u", "timer");
        up["chronometerBase"] = Value::from(phone_at - 65_000);
        let snapshot = snapshot_from(&body(Value::Array(vec![up])), desk_at);
        let u = &snapshot.items[0];
        // No `at`: the phone's own clock is taken as this one's.
        assert_eq!(
            u.live_text(&snapshot, phone_at).as_deref(),
            Some("1:05 elapsed")
        );
        assert!(
            u.visible(&snapshot, phone_at + 10 * 3_600_000),
            "a count-up stays"
        );
    }

    #[test]
    fn progress_reads_as_a_percent_and_nothing_else_has_no_live_text() {
        let mut p = item("p", "progress");
        p["progress"] = Value::from(42);
        let snapshot = snapshot_from(&body(Value::Array(vec![p, item("n", "other")])), 0);
        assert_eq!(
            snapshot.items[0].live_text(&snapshot, 5).as_deref(),
            Some("42%")
        );
        assert_eq!(snapshot.items[1].live_text(&snapshot, 5), None);
        assert_eq!(clock(3_661_000), "1:01:01");
        assert_eq!(clock(400), "0:01");
    }

    /// Shown only while the phone is ready and its notifications are shown
    /// here; an empty snapshot shows nothing.
    #[test]
    fn nothing_shows_unless_the_phone_is_ready_and_its_notifications_are_on() {
        let snapshot = snapshot_from(&body(Value::Array(vec![item("a", "timer")])), 0);
        assert_eq!(shown(Some(&snapshot), true, true, 0).len(), 1);
        assert!(shown(Some(&snapshot), false, true, 0).is_empty());
        assert!(shown(Some(&snapshot), true, false, 0).is_empty());
        assert!(shown(None, true, true, 0).is_empty());
        let empty = snapshot_from(&body(Value::Array(vec![])), 0);
        assert!(shown(Some(&empty), true, true, 0).is_empty());
    }
}
