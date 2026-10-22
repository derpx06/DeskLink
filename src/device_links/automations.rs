//! Automations (DeskLink 1.2, F7): "When … If … Do …" rules, synced with
//! each phone (contract "DeskLink 1.2 — F7 automations").
//!
//! Both apps keep every rule (last writer wins, as snippets); the side a
//! rule names in `runOn` watches its trigger and runs it. Actions of the
//! other side travel as `do`, which the other side runs only for a rule it
//! holds with that exact action. A rule a phone last changed, or a phone's
//! event fired, runs a saved command only through the same gate as the
//! phone's own run-command request.
//!
//! One worker thread owns the engine: triggers (phone events, logind's lock
//! and sleep signals, the minute clock, finished commands) reach it through
//! a bounded channel; actions that wait (logind, D-Bus, the clipboard) run
//! there, never on the GTK thread.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use super::limits::{log_limited, Buckets};
use super::packet::{NetworkPacket, PACKET_TYPE_AUTOMATIONS};

// Limits: what a peer can make this module hold or do.
/// How far ahead of this clock a rule's `modified` may be.
const MAX_FUTURE_MS: i64 = 24 * 3600 * 1000;
/// Rules kept (live and tombstones).
const MAX_RULES: usize = 200;
const MAX_ID_CHARS: usize = 128;
const MAX_NAME_CHARS: usize = 80;
const MAX_CONDITIONS: usize = 8;
const MAX_ACTIONS: usize = 8;
/// Keys in one step's params.
const MAX_PARAMS: usize = 8;
/// One string param (a URL, a notification body).
const MAX_PARAM_CHARS: usize = 512;
const MAX_KIND_CHARS: usize = 40;
/// ids per `fetch`.
const FETCH_IDS: usize = 100;
/// One `items` packet stays one data-channel message once enveloped.
const MAX_PACKET_BYTES: usize = crate::device_links::webrtc::MAX_FEATURE_PACKET_BYTES;
const TOMBSTONE_DAYS: i64 = 30;
/// Runs remembered per rule.
const HISTORY: usize = 20;
/// A rule fires at most once per this long (no loops between rules).
const REFIRE: Duration = Duration::from_secs(5);
/// Events and `do` requests accepted per phone per minute.
const EVENTS_PER_MINUTE: u32 = 60;
const DOS_PER_MINUTE: u32 = 30;
/// Messages waiting for the engine; beyond this they are dropped.
const QUEUE: usize = 64;
/// Phones whose last Wi-Fi is remembered (for the `wifi` condition).
const MAX_WIFI_PHONES: usize = 16;

pub(crate) const BUILTIN_CALL_PAUSE: &str = "builtin:call-pause";

// ── The model ──────────────────────────────────────────────────────────────

/// A trigger, condition or action: its kind and its params.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Step {
    pub(crate) kind: String,
    #[serde(default)]
    pub(crate) params: Map<String, Value>,
}

impl Step {
    pub(crate) fn new(kind: &str) -> Self {
        Self {
            kind: kind.to_string(),
            params: Map::new(),
        }
    }

    pub(crate) fn with(mut self, key: &str, value: impl Into<Value>) -> Self {
        self.params.insert(key.to_string(), value.into());
        self
    }

    pub(crate) fn text(&self, key: &str) -> Option<&str> {
        self.params.get(key).and_then(Value::as_str)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Side {
    Desk,
    Phone,
}

fn is_false(value: &bool) -> bool {
    !value
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Rule {
    pub(crate) id: String,
    #[serde(default)]
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) enabled: bool,
    pub(crate) trigger: Step,
    #[serde(default)]
    pub(crate) conditions: Vec<Step>,
    #[serde(default)]
    pub(crate) actions: Vec<Step>,
    pub(crate) run_on: Side,
    pub(crate) modified: i64,
    #[serde(default, skip_serializing_if = "is_false")]
    pub(crate) deleted: bool,
}

impl Rule {
    pub(crate) fn is_builtin(&self) -> bool {
        self.id.starts_with("builtin:")
    }

    /// Every kind is one this build knows.
    pub(crate) fn supported(&self) -> bool {
        trigger_kind(&self.trigger.kind).is_some()
            && self
                .conditions
                .iter()
                .all(|step| condition_kind(&step.kind).is_some())
            && self
                .actions
                .iter()
                .all(|step| action_kind(&step.kind).is_some())
    }

    /// "When … · If … · Do …", for the list and the top bar.
    pub(crate) fn summary(&self) -> String {
        let when = describe(&self.trigger, trigger_kind(&self.trigger.kind));
        let mut parts = vec![format!("When {when}")];
        if !self.conditions.is_empty() {
            let ifs: Vec<String> = self
                .conditions
                .iter()
                .map(|step| describe(step, condition_kind(&step.kind)))
                .collect();
            parts.push(format!("if {}", ifs.join(" and ")));
        }
        let dos: Vec<String> = self
            .actions
            .iter()
            .map(|step| describe(step, action_kind(&step.kind)))
            .collect();
        parts.push(format!("do {}", dos.join(", then ")));
        parts.join(" · ")
    }

    pub(crate) fn label(&self) -> String {
        if self.name.trim().is_empty() {
            self.summary()
        } else {
            self.name.trim().to_string()
        }
    }
}

// ── The catalog: every kind this build knows ───────────────────────────────

/// How a param is entered and checked.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Param {
    /// Free text up to the bound; `true` when it may be left empty.
    Text(bool),
    /// "HH:MM".
    Time,
    /// Day digits, Monday 1 … Sunday 7 ("" or absent = every day).
    Days,
    Number(i64, i64),
    Bool,
    Choice(&'static [(&'static str, &'static str)]),
    /// A Preferences → Run Commands id.
    Command,
}

#[derive(Debug)]
pub(crate) struct Kind {
    pub(crate) id: &'static str,
    /// What the editor lists ("Phone connects").
    pub(crate) label: &'static str,
    /// Where it happens (triggers) or runs (actions).
    pub(crate) side: Side,
    pub(crate) params: &'static [(&'static str, &'static str, Param)],
}

const WIFI_STATES: &[(&str, &str)] = &[("connected", "connects to"), ("disconnected", "leaves")];
const CHARGING: &[(&str, &str)] = &[("start", "starts"), ("stop", "stops")];
const CALLS: &[(&str, &str)] = &[("incoming", "comes in"), ("missed", "is missed")];
const RINGER: &[(&str, &str)] = &[
    ("normal", "Ring"),
    ("vibrate", "Vibrate"),
    ("silent", "Silent"),
];
const STREAMS: &[(&str, &str)] = &[("media", "Media"), ("ring", "Ring"), ("alarm", "Alarm")];
const SCREENS: &[(&str, &str)] = &[("terminals", "Terminals"), ("desk-screen", "Desk screen")];

pub(crate) const TRIGGERS: &[Kind] = &[
    Kind {
        id: "phone-connected",
        label: "The phone connects",
        side: Side::Desk,
        params: &[],
    },
    Kind {
        id: "phone-left",
        label: "The phone leaves",
        side: Side::Desk,
        params: &[],
    },
    Kind {
        id: "desk-locked",
        label: "This computer locks",
        side: Side::Desk,
        params: &[],
    },
    Kind {
        id: "desk-unlocked",
        label: "This computer unlocks",
        side: Side::Desk,
        params: &[],
    },
    Kind {
        id: "desk-sleep",
        label: "This computer goes to sleep",
        side: Side::Desk,
        params: &[],
    },
    Kind {
        id: "desk-resume",
        label: "This computer wakes up",
        side: Side::Desk,
        params: &[],
    },
    Kind {
        id: "time",
        label: "At a time",
        side: Side::Desk,
        params: &[("at", "Time", Param::Time), ("days", "Days", Param::Days)],
    },
    Kind {
        id: "command-finished",
        label: "A long command finishes",
        side: Side::Desk,
        params: &[("job", "Program (any if empty)", Param::Text(true))],
    },
    Kind {
        id: "wifi",
        label: "The phone's Wi-Fi changes",
        side: Side::Phone,
        params: &[
            ("state", "When it", Param::Choice(WIFI_STATES)),
            ("ssid", "Network (any if empty)", Param::Text(true)),
        ],
    },
    Kind {
        id: "charging",
        label: "The phone's charging",
        side: Side::Phone,
        params: &[("state", "When it", Param::Choice(CHARGING))],
    },
    Kind {
        id: "battery",
        label: "The phone's battery runs low",
        side: Side::Phone,
        params: &[("below", "Below (%)", Param::Number(1, 100))],
    },
    Kind {
        id: "call",
        label: "A call",
        side: Side::Phone,
        params: &[("state", "When it", Param::Choice(CALLS))],
    },
    Kind {
        id: "notification",
        label: "A notification from an app",
        side: Side::Phone,
        params: &[("app", "App package", Param::Text(false))],
    },
    Kind {
        id: "nfc",
        label: "An NFC tag is tapped",
        side: Side::Phone,
        params: &[("tag", "Tag name", Param::Text(false))],
    },
];

pub(crate) const CONDITIONS: &[Kind] = &[
    Kind {
        id: "phone-connected",
        label: "The phone is connected",
        side: Side::Desk,
        params: &[("connected", "Connected", Param::Bool)],
    },
    Kind {
        id: "wifi",
        label: "The phone is on a Wi-Fi network",
        side: Side::Phone,
        params: &[("ssid", "Network", Param::Text(false))],
    },
    Kind {
        id: "time-between",
        label: "The time is between",
        side: Side::Desk,
        params: &[("from", "From", Param::Time), ("to", "To", Param::Time)],
    },
    Kind {
        id: "desk-locked",
        label: "This computer is locked",
        side: Side::Desk,
        params: &[("locked", "Locked", Param::Bool)],
    },
];

pub(crate) const ACTIONS: &[Kind] = &[
    Kind {
        id: "run-command",
        label: "Run a saved command",
        side: Side::Desk,
        params: &[("commandId", "Command", Param::Command)],
    },
    Kind {
        id: "lock-desk",
        label: "Lock this computer",
        side: Side::Desk,
        params: &[],
    },
    Kind {
        id: "keep-awake",
        label: "Keep this computer awake",
        side: Side::Desk,
        params: &[("on", "On", Param::Bool)],
    },
    Kind {
        id: "send-clipboard",
        label: "Send the clipboard to the phone",
        side: Side::Desk,
        params: &[],
    },
    Kind {
        id: "open-url",
        label: "Open a web page here",
        side: Side::Desk,
        params: &[("url", "Address (https://…)", Param::Text(false))],
    },
    Kind {
        id: "notify",
        label: "Show a notification here",
        side: Side::Desk,
        params: &[
            ("title", "Title", Param::Text(false)),
            ("body", "Text", Param::Text(true)),
        ],
    },
    Kind {
        id: "pause-media",
        label: "Pause music here",
        side: Side::Desk,
        params: &[],
    },
    Kind {
        id: "phone-ringer",
        label: "Set the phone's ringer",
        side: Side::Phone,
        params: &[("mode", "Mode", Param::Choice(RINGER))],
    },
    Kind {
        id: "phone-ring",
        label: "Ring the phone",
        side: Side::Phone,
        params: &[],
    },
    Kind {
        id: "phone-volume",
        label: "Set a phone volume",
        side: Side::Phone,
        params: &[
            ("stream", "Volume", Param::Choice(STREAMS)),
            ("percent", "Level (%)", Param::Number(0, 100)),
        ],
    },
    Kind {
        id: "phone-dnd",
        label: "Set the phone's Do Not Disturb",
        side: Side::Phone,
        params: &[("on", "On", Param::Bool)],
    },
    Kind {
        id: "phone-notify",
        label: "Show a notification on the phone",
        side: Side::Phone,
        params: &[
            ("title", "Title", Param::Text(false)),
            ("body", "Text", Param::Text(true)),
        ],
    },
    Kind {
        id: "phone-open",
        label: "Open a DeskLink screen on the phone",
        side: Side::Phone,
        params: &[("screen", "Screen", Param::Choice(SCREENS))],
    },
];

fn find(list: &'static [Kind], id: &str) -> Option<&'static Kind> {
    list.iter().find(|kind| kind.id == id)
}

pub(crate) fn trigger_kind(id: &str) -> Option<&'static Kind> {
    find(TRIGGERS, id)
}

pub(crate) fn condition_kind(id: &str) -> Option<&'static Kind> {
    find(CONDITIONS, id)
}

pub(crate) fn action_kind(id: &str) -> Option<&'static Kind> {
    find(ACTIONS, id)
}

/// "the phone's Wi-Fi changes (connected, Home)" — a step in words.
fn describe(step: &Step, kind: Option<&Kind>) -> String {
    let Some(kind) = kind else {
        return format!("“{}” (needs a newer DeskLink)", step.kind);
    };
    // "The phone's Wi-Fi changes" → "the phone's Wi-Fi changes".
    let mut chars = kind.label.chars();
    let label: String = chars
        .next()
        .map(|first| first.to_lowercase().chain(chars).collect())
        .unwrap_or_default();
    let values: Vec<String> = kind
        .params
        .iter()
        .filter_map(|(key, _, param)| {
            let value = step.params.get(*key)?;
            Some(match (param, value) {
                (Param::Choice(choices), Value::String(text)) => choices
                    .iter()
                    .find(|(id, _)| id == text)
                    .map_or(text.clone(), |(_, words)| words.to_string()),
                (Param::Bool, Value::Bool(on)) => if *on { "on" } else { "off" }.to_string(),
                (Param::Command, Value::String(id)) => command_name(id),
                (Param::Days, Value::String(days)) if days.is_empty() => return None,
                (Param::Days, Value::String(days)) => day_names(days),
                (_, Value::String(text)) if text.is_empty() => return None,
                (_, Value::String(text)) => text.chars().take(40).collect(),
                (_, other) => other.to_string(),
            })
        })
        .collect();
    if values.is_empty() {
        label
    } else {
        format!("{label} ({})", values.join(", "))
    }
}

fn command_name(id: &str) -> String {
    super::config::app_settings()
        .run_commands
        .into_iter()
        .find(|command| command.id == id)
        .map_or_else(|| "a removed command".to_string(), |command| command.name)
}

fn day_names(days: &str) -> String {
    const NAMES: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
    days.chars()
        .filter_map(|day| day.to_digit(10))
        .filter(|day| (1..=7).contains(day))
        .map(|day| NAMES[day as usize - 1])
        .collect::<Vec<_>>()
        .join(" ")
}

// ── Validation (on receipt and on save; over a bound is dropped) ───────────

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.chars().count() <= MAX_ID_CHARS && !id.chars().any(char::is_control)
}

fn valid_step(step: &Step) -> bool {
    !step.kind.is_empty()
        && step.kind.len() <= MAX_KIND_CHARS
        && step.params.len() <= MAX_PARAMS
        && step.params.iter().all(|(key, value)| {
            key.len() <= MAX_KIND_CHARS
                && match value {
                    Value::String(text) => text.chars().count() <= MAX_PARAM_CHARS,
                    Value::Number(_) | Value::Bool(_) => true,
                    _ => false,
                }
        })
}

pub(crate) fn valid_rule(rule: &Rule) -> bool {
    valid_id(&rule.id)
        && rule.name.chars().count() <= MAX_NAME_CHARS
        && rule.modified > 0
        // A clock a little off is fine; a version from far ahead would win
        // every merge forever.
        && rule.modified <= now_ms().saturating_add(MAX_FUTURE_MS)
        && rule.conditions.len() <= MAX_CONDITIONS
        && rule.actions.len() <= MAX_ACTIONS
        && (rule.deleted || !rule.actions.is_empty())
        && valid_step(&rule.trigger)
        && rule.conditions.iter().all(valid_step)
        && rule.actions.iter().all(valid_step)
}

/// Why a rule from the editor cannot be saved, in plain words.
pub(crate) fn check_params(step: &Step, kind: &Kind) -> Result<(), String> {
    for (key, label, param) in kind.params {
        let value = step.params.get(*key);
        let ok = match (param, value) {
            (Param::Text(optional), None) => *optional,
            (Param::Text(optional), Some(Value::String(text))) => {
                (*optional || !text.trim().is_empty()) && text.chars().count() <= MAX_PARAM_CHARS
            }
            (Param::Time, Some(Value::String(text))) => parse_time(text).is_some(),
            (Param::Days, None) => true,
            (Param::Days, Some(Value::String(days))) => {
                days.chars().all(|day| ('1'..='7').contains(&day))
            }
            (Param::Number(low, high), Some(value)) => value
                .as_i64()
                .is_some_and(|number| (*low..=*high).contains(&number)),
            (Param::Bool, Some(Value::Bool(_))) => true,
            (Param::Choice(choices), Some(Value::String(text))) => {
                choices.iter().any(|(id, _)| id == text)
            }
            (Param::Command, Some(Value::String(id))) => super::config::app_settings()
                .run_commands
                .iter()
                .any(|command| &command.id == id),
            _ => false,
        };
        if !ok {
            return Err(format!("{}: “{label}” needs a value", kind.label));
        }
    }
    if kind.id == "open-url" && step.text("url").is_some_and(|url| !web_address(url)) {
        return Err("The address must start with https:// or http://".into());
    }
    Ok(())
}

fn web_address(url: &str) -> bool {
    (url.starts_with("https://") || url.starts_with("http://"))
        && url.len() > 8
        && !url.chars().any(|c| c.is_control() || c.is_whitespace())
}

fn parse_time(text: &str) -> Option<(u32, u32)> {
    let (hours, minutes) = text.split_once(':')?;
    let (hours, minutes) = (hours.parse().ok()?, minutes.parse().ok()?);
    (hours < 24 && minutes < 60 && text.len() == 5).then_some((hours, minutes))
}

// ── Matching (A6) ──────────────────────────────────────────────────────────

/// Whether `rule`'s trigger matches `event` (kinds equal; every param the
/// rule sets agrees).
fn matches(rule: &Step, event: &Step) -> bool {
    if rule.kind != event.kind {
        return false;
    }
    rule.params.iter().all(|(key, wanted)| {
        let got = event.params.get(key);
        match (rule.kind.as_str(), key.as_str()) {
            ("time", "days") => match (wanted.as_str(), event.text("day")) {
                (Some(""), _) => true,
                (Some(days), Some(day)) => days.contains(day),
                _ => false,
            },
            ("battery", "below") => match (wanted.as_i64(), got.and_then(Value::as_i64)) {
                (Some(wanted), Some(got)) => got <= wanted,
                _ => false,
            },
            ("notification", "app") | ("command-finished", "job") | ("nfc", "tag") => {
                wanted.as_str() == Some("") || got == Some(wanted)
            }
            _ => match (wanted, got) {
                (Value::String(wanted), _) if wanted.is_empty() => true,
                (Value::String(wanted), Some(Value::String(got))) => {
                    wanted.eq_ignore_ascii_case(got)
                }
                (wanted, Some(got)) => wanted == got,
                (_, None) => false,
            },
        }
    })
}

/// What conditions are checked against.
#[derive(Debug, Clone, Default)]
struct World {
    connected: bool,
    /// Phones' last reported Wi-Fi network (empty: none).
    wifi: Vec<String>,
    /// Minutes since midnight, local.
    minute: u32,
    locked: Option<bool>,
}

fn holds(condition: &Step, world: &World) -> bool {
    match condition.kind.as_str() {
        "phone-connected" => {
            condition
                .params
                .get("connected")
                .and_then(Value::as_bool)
                .unwrap_or(true)
                == world.connected
        }
        "wifi" => condition.text("ssid").is_some_and(|ssid| {
            world
                .wifi
                .iter()
                .any(|known| known.eq_ignore_ascii_case(ssid))
        }),
        "time-between" => {
            let minutes = |key| {
                condition
                    .text(key)
                    .and_then(parse_time)
                    .map(|(h, m)| h * 60 + m)
            };
            match (minutes("from"), minutes("to")) {
                (Some(from), Some(to)) if from <= to => (from..to).contains(&world.minute),
                // Wraps midnight.
                (Some(from), Some(to)) => world.minute >= from || world.minute < to,
                _ => false,
            }
        }
        "desk-locked" => {
            world.locked
                == Some(
                    condition
                        .params
                        .get("locked")
                        .and_then(Value::as_bool)
                        .unwrap_or(true),
                )
        }
        _ => false,
    }
}

// ── The store ──────────────────────────────────────────────────────────────

/// One run of a rule, for its history.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Run {
    pub(crate) id: String,
    /// Unix ms.
    pub(crate) at: i64,
    /// What fired it ("Test run", "Pixel 9: Wi-Fi connected").
    pub(crate) cause: String,
    /// One line per action: "Locked this computer", "Refused: …".
    pub(crate) results: Vec<String>,
    pub(crate) ok: bool,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct Shelf {
    /// Schema version of this file.
    #[serde(default)]
    version: u32,
    rules: BTreeMap<String, Rule>,
    /// Who last changed each rule: "desk" or a phone's device id. Local.
    #[serde(default)]
    origin: BTreeMap<String, String>,
    #[serde(default)]
    history: BTreeMap<String, VecDeque<Run>>,
}

const SCHEMA: u32 = 1;
const FILE: &str = "rules.sealed";
const AAD: &[u8] = b"desklink-automations";
const DESK: &str = "desk";

struct Store {
    dir: PathBuf,
    shelf: Shelf,
}

fn store() -> &'static Mutex<Option<Store>> {
    static STORE: OnceLock<Mutex<Option<Store>>> = OnceLock::new();
    STORE.get_or_init(Default::default)
}

static GENERATION: AtomicU64 = AtomicU64::new(0);

/// Bumped on every change, for the UI to redraw.
pub(crate) fn generation() -> u64 {
    GENERATION.load(Ordering::Relaxed)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

fn builtin_call_pause() -> Rule {
    Rule {
        id: BUILTIN_CALL_PAUSE.into(),
        name: "When a call comes in, pause music".into(),
        enabled: true,
        trigger: Step::new("call").with("state", "incoming"),
        conditions: Vec::new(),
        actions: vec![Step::new("pause-media")],
        run_on: Side::Desk,
        modified: 1,
        deleted: false,
    }
}

/// Loads the rules from `dir` (`<config>/automations`). Called once at
/// startup. An unreadable file is set aside, never fatal.
pub(crate) fn install(dir: PathBuf) {
    let mut shelf: Shelf = super::vault::open_or_quarantine(&dir, &dir.join(FILE), AAD);
    shelf.version = SCHEMA;
    shelf.rules.retain(|_, rule| valid_rule(rule));
    shelf
        .rules
        .entry(BUILTIN_CALL_PAUSE.into())
        .or_insert_with(builtin_call_pause);
    prune(&mut shelf, now_ms());
    if let Ok(mut store) = store().lock() {
        *store = Some(Store { dir, shelf });
    }
    GENERATION.fetch_add(1, Ordering::Relaxed);
}

fn prune(shelf: &mut Shelf, now: i64) {
    let cutoff = now - TOMBSTONE_DAYS * 24 * 3600 * 1000;
    shelf
        .rules
        .retain(|_, rule| !rule.deleted || rule.modified > cutoff);
    // Over the bound: the oldest tombstones go first, then the oldest rules.
    while shelf.rules.len() > MAX_RULES {
        let oldest = shelf
            .rules
            .values()
            .filter(|rule| !rule.is_builtin())
            .min_by_key(|rule| (!rule.deleted, rule.modified))
            .map(|rule| rule.id.clone());
        match oldest {
            Some(id) => {
                shelf.rules.remove(&id);
            }
            None => break,
        }
    }
    let rules = &shelf.rules;
    shelf.origin.retain(|id, _| rules.contains_key(id));
    shelf.history.retain(|id, _| rules.contains_key(id));
}

/// Runs `job` on the shelf; saves when it says it changed it.
fn with_shelf<T>(job: impl FnOnce(&mut Shelf) -> (T, bool)) -> Option<T> {
    let mut guard = store().lock().ok()?;
    let store = guard.as_mut()?;
    let (result, changed) = job(&mut store.shelf);
    if changed {
        // Sealed and written off this thread (often the GTK one).
        match serde_json::to_vec(&store.shelf) {
            Ok(plain) => super::vault::seal_later(&store.dir, &store.dir.join(FILE), AAD, plain),
            Err(_) => eprintln!("[Automations] Could not save the rules"),
        }
        GENERATION.fetch_add(1, Ordering::Relaxed);
    }
    Some(result)
}

// ── Local API (the window) ─────────────────────────────────────────────────

/// Live rules: built-ins first, then by name.
pub(crate) fn rules() -> Vec<Rule> {
    let mut list: Vec<Rule> = with_shelf(|shelf| {
        (
            shelf
                .rules
                .values()
                .filter(|rule| !rule.deleted)
                .cloned()
                .collect(),
            false,
        )
    })
    .unwrap_or_default();
    list.sort_by_key(|rule| (!rule.is_builtin(), rule.label().to_lowercase()));
    list
}

pub(crate) fn history(id: &str) -> Vec<Run> {
    with_shelf(|shelf| {
        (
            shelf
                .history
                .get(id)
                .map(|runs| runs.iter().rev().cloned().collect())
                .unwrap_or_default(),
            false,
        )
    })
    .unwrap_or_default()
}

/// Whether a built-in rule is on (the call media hold asks).
pub(crate) fn builtin_enabled(id: &str) -> bool {
    with_shelf(|shelf| {
        (
            shelf
                .rules
                .get(id)
                .is_none_or(|rule| rule.enabled && !rule.deleted),
            false,
        )
    })
    .unwrap_or(true)
}

/// Saves `rule` from the editor (new or changed). Plain words on refusal.
pub(crate) fn save_rule(mut rule: Rule) -> Result<(), String> {
    if rule.actions.is_empty() {
        return Err("Add at least one action".into());
    }
    let trigger = trigger_kind(&rule.trigger.kind).ok_or("Choose what starts it")?;
    check_params(&rule.trigger, trigger)?;
    for step in &rule.conditions {
        check_params(step, condition_kind(&step.kind).ok_or("Unknown condition")?)?;
    }
    for step in &rule.actions {
        check_params(step, action_kind(&step.kind).ok_or("Unknown action")?)?;
    }
    if rule.id.is_empty() {
        rule.id = uuid::Uuid::new_v4().to_string();
    }
    rule.name = rule.name.trim().chars().take(MAX_NAME_CHARS).collect();
    rule.deleted = false;
    // Set again under the lock (later than the version it replaces).
    rule.modified = now_ms();
    if !valid_rule(&rule) {
        return Err("This rule is too large".into());
    }
    let id = rule.id.clone();
    let saved = with_shelf(|shelf| {
        let live = shelf.rules.values().filter(|rule| !rule.deleted).count();
        let previous = shelf.rules.get(&rule.id);
        if previous.is_some_and(Rule::is_builtin) {
            return (Err("A built-in rule can only be turned on or off"), false);
        }
        if previous.is_none() && live >= MAX_RULES {
            return (Err("Too many rules (200 at most)"), false);
        }
        rule.modified = now_ms().max(previous.map_or(0, |rule| rule.modified.saturating_add(1)));
        shelf.origin.insert(rule.id.clone(), DESK.into());
        shelf.rules.insert(rule.id.clone(), rule);
        (Ok(()), true)
    })
    .ok_or("Automations are not available")?;
    saved?;
    push(vec![id]);
    Ok(())
}

pub(crate) fn set_enabled(id: &str, enabled: bool) {
    let changed = with_shelf(|shelf| match shelf.rules.get_mut(id) {
        Some(rule) if rule.enabled != enabled && !rule.deleted => {
            rule.enabled = enabled;
            rule.modified = now_ms().max(rule.modified.saturating_add(1));
            // Toggling is not editing: who last changed the rule (and so
            // whether it needs approval or a gate) stays as it was.
            (true, true)
        }
        _ => (false, false),
    });
    if changed == Some(true) {
        push(vec![id.to_string()]);
    }
}

pub(crate) fn delete_rule(id: &str) {
    let changed = with_shelf(|shelf| match shelf.rules.get_mut(id) {
        Some(rule) if !rule.deleted && !rule.is_builtin() => {
            rule.deleted = true;
            rule.conditions.clear();
            rule.actions.clear();
            rule.modified = now_ms().max(rule.modified.saturating_add(1));
            shelf.history.remove(id);
            (true, true)
        }
        _ => (false, false),
    });
    if changed == Some(true) {
        push(vec![id.to_string()]);
    }
}

/// "Test run": the actions now, conditions skipped.
pub(crate) fn test_run(id: &str) {
    send_message(Message::Test(id.to_string()));
}

// ── Sync (A2) ──────────────────────────────────────────────────────────────

/// A `desklink.automations` packet from `device_id`: what to answer.
pub(crate) fn on_packet(device_id: &str, packet: &NetworkPacket) -> Vec<NetworkPacket> {
    match packet.get_str("op") {
        Some("index") => with_shelf(|shelf| {
            let items = packet
                .body
                .get("items")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or_default();
            let wanted = wanted(shelf, items);
            // Ours the peer lacks or has older: send them unasked.
            let theirs: HashMap<&str, i64> = items
                .iter()
                .filter_map(|item| {
                    Some((item.get("id")?.as_str()?, item.get("modified")?.as_i64()?))
                })
                .collect();
            let newer: Vec<String> = shelf
                .rules
                .values()
                .filter(|rule| {
                    theirs
                        .get(rule.id.as_str())
                        .is_none_or(|t| *t < rule.modified)
                })
                .map(|rule| rule.id.clone())
                .collect();
            let mut answers: Vec<NetworkPacket> = wanted
                .chunks(FETCH_IDS)
                .map(|ids| {
                    let mut fetch = NetworkPacket::new(PACKET_TYPE_AUTOMATIONS);
                    fetch.set("op", "fetch");
                    fetch.set("ids", ids.to_vec());
                    fetch
                })
                .collect();
            answers.extend(items_packets(item_values(shelf, &newer)));
            (answers, false)
        })
        .unwrap_or_default(),
        Some("fetch") => with_shelf(|shelf| {
            let ids: Vec<String> = packet
                .body
                .get("ids")
                .and_then(Value::as_array)
                .map(|ids| {
                    ids.iter()
                        .take(FETCH_IDS)
                        .filter_map(|id| id.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            (items_packets(item_values(shelf, &ids)), false)
        })
        .unwrap_or_default(),
        Some("items") => {
            let items: Vec<Rule> = packet
                .body
                .get("items")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .take(MAX_RULES)
                        .filter_map(|item| serde_json::from_value::<Rule>(item.clone()).ok())
                        .filter(valid_rule)
                        .collect()
                })
                .unwrap_or_default();
            with_shelf(|shelf| {
                let mut taken = false;
                for rule in items {
                    taken |= merge(shelf, rule, device_id);
                }
                if taken {
                    prune(shelf, now_ms());
                }
                ((), taken)
            });
            Vec::new()
        }
        Some("event") => {
            if !EVENT_BUCKETS.allow(device_id) {
                log_limited("events", "Too many automation events from a phone; dropped");
                return Vec::new();
            }
            let trigger = packet
                .body
                .get("trigger")
                .and_then(|value| serde_json::from_value::<Step>(value.clone()).ok())
                .filter(valid_step);
            if let Some(trigger) = trigger {
                send_message(Message::Event {
                    from: Some(device_id.to_string()),
                    trigger,
                });
            }
            Vec::new()
        }
        Some("do") => {
            if !DO_BUCKETS.allow(device_id) {
                return vec![done_packet(
                    packet.get_str("runId").unwrap_or(""),
                    Err("busy"),
                )];
            }
            let (Some(rule_id), Some(run_id), Some(action)) = (
                packet.get_str("ruleId"),
                packet.get_str("runId").filter(|id| valid_id(id)),
                packet
                    .body
                    .get("action")
                    .and_then(|value| serde_json::from_value::<Step>(value.clone()).ok())
                    .filter(valid_step),
            ) else {
                return Vec::new();
            };
            send_message(Message::Do {
                device: device_id.to_string(),
                rule_id: rule_id.to_string(),
                run_id: run_id.to_string(),
                action,
            });
            Vec::new()
        }
        Some("done") => {
            if let (Some(run_id), Some(ok)) = (packet.get_str("runId"), packet.get_bool("ok")) {
                let error = packet.get_str("error").unwrap_or("failed");
                record_done(run_id, ok, error);
            }
            Vec::new()
        }
        _ => Vec::new(),
    }
}

/// Last writer wins (equal keeps ours). Built-ins take only `enabled`.
fn merge(shelf: &mut Shelf, incoming: Rule, device_id: &str) -> bool {
    match shelf.rules.get_mut(&incoming.id) {
        Some(local) if local.modified >= incoming.modified => false,
        Some(local) if local.is_builtin() => {
            local.enabled = incoming.enabled;
            local.modified = incoming.modified;
            true
        }
        _ if incoming.is_builtin() => false,
        _ => {
            if incoming.deleted {
                shelf.history.remove(&incoming.id);
            }
            shelf
                .origin
                .insert(incoming.id.clone(), device_id.to_string());
            shelf.rules.insert(incoming.id.clone(), incoming);
            true
        }
    }
}

fn wanted(shelf: &Shelf, index: &[Value]) -> Vec<String> {
    index
        .iter()
        .take(MAX_RULES * 2)
        .filter_map(|entry| {
            let id = entry.get("id")?.as_str()?;
            let modified = entry.get("modified")?.as_i64()?;
            let fetch = valid_id(id)
                && shelf
                    .rules
                    .get(id)
                    .is_none_or(|rule| modified > rule.modified);
            fetch.then(|| id.to_string())
        })
        .collect()
}

fn index_packet(shelf: &Shelf) -> NetworkPacket {
    let items: Vec<Value> = shelf
        .rules
        .values()
        .map(|rule| json!({"id": rule.id, "modified": rule.modified, "deleted": rule.deleted}))
        .collect();
    let mut packet = NetworkPacket::new(PACKET_TYPE_AUTOMATIONS);
    packet.set("op", "index");
    packet.set("items", items);
    packet
}

fn item_values(shelf: &Shelf, ids: &[String]) -> Vec<Value> {
    ids.iter()
        .filter_map(|id| shelf.rules.get(id))
        .filter_map(|rule| serde_json::to_value(rule).ok())
        .collect()
}

/// `items` packets for `values`, split under [`MAX_PACKET_BYTES`].
fn items_packets(values: Vec<Value>) -> Vec<NetworkPacket> {
    let mut packets = Vec::new();
    let mut batch: Vec<Value> = Vec::new();
    let mut size = 0;
    let flush = |batch: &mut Vec<Value>, packets: &mut Vec<NetworkPacket>| {
        if !batch.is_empty() {
            let mut packet = NetworkPacket::new(PACKET_TYPE_AUTOMATIONS);
            packet.set("op", "items");
            packet.set("items", std::mem::take(batch));
            packets.push(packet);
        }
    };
    for value in values {
        let length = value.to_string().len();
        if length > MAX_PACKET_BYTES {
            log_limited("rule-size", "A rule too large to sync was skipped");
            continue;
        }
        if size + length > MAX_PACKET_BYTES {
            flush(&mut batch, &mut packets);
            size = 0;
        }
        size += length;
        batch.push(value);
    }
    flush(&mut batch, &mut packets);
    packets
}

/// The index, for a phone whose connection just came up.
pub(crate) fn index() -> Option<NetworkPacket> {
    with_shelf(|shelf| (index_packet(shelf), false))
}

/// Sends rules `ids` to every connected phone (a local change).
fn push(ids: Vec<String>) {
    let Some(packets) = with_shelf(|shelf| (items_packets(item_values(shelf, &ids)), false)) else {
        return;
    };
    super::desk_features::run_later(move || {
        for device in super::desk_features::ready_devices() {
            for packet in &packets {
                super::desk_features::send(&device, packet);
            }
        }
    });
}

// ── Rate limits and logs ───────────────────────────────────────────────────

static EVENT_BUCKETS: Buckets = Buckets::new(EVENTS_PER_MINUTE, Duration::from_secs(60));
static DO_BUCKETS: Buckets = Buckets::new(DOS_PER_MINUTE, Duration::from_secs(60));

// ── The engine ─────────────────────────────────────────────────────────────

#[derive(Debug)]
enum Message {
    /// A trigger happened: here (`from` None) or on that phone.
    Event {
        from: Option<String>,
        trigger: Step,
    },
    Do {
        device: String,
        rule_id: String,
        run_id: String,
        action: Step,
    },
    Test(String),
}

fn queue() -> &'static OnceLock<SyncSender<Message>> {
    static QUEUE_SENDER: OnceLock<SyncSender<Message>> = OnceLock::new();
    &QUEUE_SENDER
}

fn send_message(message: Message) {
    if let Some(sender) = queue().get() {
        if sender.try_send(message).is_err() {
            log_limited("queue", "Automations are busy; an event was dropped");
        }
    }
}

/// A trigger happened on this computer (A5). Phones hear about it too.
pub(crate) fn fire(trigger: Step) {
    send_message(Message::Event {
        from: None,
        trigger,
    });
}

/// The link to `device_id` came up or went down.
pub(crate) fn phone_link(device_id: &str, ready: bool) {
    let trigger = Step::new(if ready {
        "phone-connected"
    } else {
        "phone-left"
    });
    if ready {
        // The phone hears it (about itself), after the index.
        let mut packet = NetworkPacket::new(PACKET_TYPE_AUTOMATIONS);
        packet.set("op", "event");
        packet.set("trigger", json!(trigger));
        super::desk_features::send(device_id, &packet);
    } else if let Ok(mut wifi) = WIFI.lock() {
        if let Some(wifi) = wifi.as_mut() {
            wifi.remove(device_id);
        }
    }
    send_message(Message::Event {
        from: Some(device_id.to_string()),
        trigger,
    });
}

/// Starts the engine (once, at daemon start).
pub(crate) fn start() {
    let (sender, receiver) = mpsc::sync_channel(QUEUE);
    if queue().set(sender).is_err() {
        return;
    }
    let _ = std::thread::Builder::new()
        .name("desklink-automations".into())
        .spawn(move || {
            let context = gtk::glib::MainContext::new();
            let _ = context.with_thread_default(|| engine(&context, &receiver));
        });
}

/// Each phone's last Wi-Fi network, from its events.
static WIFI: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);

fn remember_wifi(device_id: &str, trigger: &Step) {
    if trigger.kind != "wifi" {
        return;
    }
    let Ok(mut wifi) = WIFI.lock() else {
        return;
    };
    let wifi = wifi.get_or_insert_with(HashMap::new);
    match (trigger.text("state"), trigger.text("ssid")) {
        (Some("connected"), Some(ssid))
            if wifi.len() < MAX_WIFI_PHONES || wifi.contains_key(device_id) =>
        {
            wifi.insert(device_id.to_string(), ssid.to_string());
        }
        (Some("disconnected"), _) => {
            wifi.remove(device_id);
        }
        _ => {}
    }
}

fn world() -> World {
    let now = gtk::glib::DateTime::now_local().ok();
    World {
        connected: !super::desk_features::ready_devices().is_empty(),
        wifi: WIFI
            .lock()
            .ok()
            .and_then(|wifi| wifi.as_ref().map(|wifi| wifi.values().cloned().collect()))
            .unwrap_or_default(),
        minute: now.map_or(0, |now| (now.hour() * 60 + now.minute()) as u32),
        locked: super::logind::locked(),
    }
}

/// Who can be named as having fired a rule, for the history.
fn device_name(device_id: &str) -> String {
    let name = super::connection_log::name_of(device_id);
    if name.is_empty() {
        "The phone".to_string()
    } else {
        name
    }
}

fn cause(from: Option<&str>, trigger: &Step) -> String {
    let what = describe(trigger, trigger_kind(&trigger.kind));
    match from {
        Some(device) => format!("{}: {what}", device_name(device)),
        None => what,
    }
}

fn engine(context: &gtk::glib::MainContext, messages: &Receiver<Message>) {
    let _signals = subscribe_logind();
    let mut fired: HashMap<String, Instant> = HashMap::new();
    let mut minute_seen = String::new();
    loop {
        let message = match messages.recv_timeout(Duration::from_millis(500)) {
            Ok(message) => Some(message),
            Err(mpsc::RecvTimeoutError::Timeout) => None,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        while context.pending() {
            context.iteration(false);
        }
        // The minute clock (A5 `time`).
        if let Ok(now) = gtk::glib::DateTime::now_local() {
            let at = format!("{:02}:{:02}", now.hour(), now.minute());
            if at != minute_seen {
                let first = minute_seen.is_empty();
                minute_seen = at.clone();
                if !first {
                    let day = now.day_of_week().to_string();
                    run_matching(
                        None,
                        &Step::new("time").with("at", at).with("day", day),
                        &mut fired,
                    );
                }
            }
        }
        match message {
            Some(Message::Event { from, trigger }) => {
                if let Some(device) = from.as_deref() {
                    remember_wifi(device, &trigger);
                } else if trigger.kind != "time" {
                    // A5: phones hear this computer's triggers.
                    let mut packet = NetworkPacket::new(PACKET_TYPE_AUTOMATIONS);
                    packet.set("op", "event");
                    packet.set("trigger", json!(trigger));
                    for device in super::desk_features::ready_devices() {
                        super::desk_features::send(&device, &packet);
                    }
                }
                run_matching(from.as_deref(), &trigger, &mut fired);
            }
            Some(Message::Do {
                device,
                rule_id,
                run_id,
                action,
            }) => {
                let result = run_requested(&device, &rule_id, &action);
                record_run(
                    &rule_id,
                    Run {
                        id: run_id.clone(),
                        at: now_ms(),
                        cause: format!("{} asked", device_name(&device)),
                        results: vec![match &result {
                            Ok(done) => done.clone(),
                            Err(error) => format!("Refused: {error}"),
                        }],
                        ok: result.is_ok(),
                    },
                );
                let answer = done_packet(&run_id, result.as_ref().map(|_| ()).map_err(|e| code(e)));
                super::desk_features::send(&device, &answer);
            }
            Some(Message::Test(id)) => {
                if let Some(rule) = rules().into_iter().find(|rule| rule.id == id) {
                    run_rule(&rule, None, "Test run".into());
                }
            }
            None => {}
        }
    }
}

/// Runs this computer's rules that `trigger` matches.
fn run_matching(from: Option<&str>, trigger: &Step, fired: &mut HashMap<String, Instant>) {
    let candidates: Vec<Rule> = rules()
        .into_iter()
        .filter(|rule| {
            rule.enabled
                && rule.run_on == Side::Desk
                && !rule.is_builtin()
                && rule.supported()
                && matches(&rule.trigger, trigger)
        })
        .collect();
    if candidates.is_empty() {
        return;
    }
    let world = world();
    for rule in candidates {
        if !rule
            .conditions
            .iter()
            .all(|condition| holds(condition, &world))
        {
            continue;
        }
        if fired.get(&rule.id).is_some_and(|at| at.elapsed() < REFIRE) {
            log_limited("refire", "A rule fired again too soon; skipped");
            continue;
        }
        fired.insert(rule.id.clone(), Instant::now());
        if fired.len() > MAX_RULES * 2 {
            fired.retain(|_, at| at.elapsed() < REFIRE);
        }
        run_rule(&rule, from, cause(from, trigger));
    }
}

/// The rule's actions in order: here, or asked of the phones.
fn run_rule(rule: &Rule, from: Option<&str>, cause: String) {
    let approved = approved_here(&rule.id);
    // A phone fired it: that phone's gate applies to a saved command.
    let gate = from.map(str::to_string);
    let run_id = uuid::Uuid::new_v4().to_string();
    let mut results = Vec::new();
    let mut ok = true;
    for action in &rule.actions {
        let Some(kind) = action_kind(&action.kind) else {
            results.push("Skipped: needs a newer DeskLink".to_string());
            ok = false;
            continue;
        };
        let result = match kind.side {
            // A rule a phone made or changed acts here only once approved
            // here: syncing a rule must not be a way to run desk actions.
            Side::Desk if !approved => Err(NOT_APPROVED.to_string()),
            Side::Desk => run_action(action, gate.as_deref()),
            Side::Phone => ask_phones(&rule.id, &run_id, action),
        };
        match result {
            Ok(done) => results.push(done),
            Err(error) => {
                results.push(format!("Failed: {error}"));
                ok = false;
            }
        }
    }
    debug_log!(
        "[Automations] Rule ran: {} actions, ok {ok}",
        rule.actions.len()
    );
    record_run(
        &rule.id,
        Run {
            id: run_id,
            at: now_ms(),
            cause,
            results,
            ok,
        },
    );
}

fn ask_phones(rule_id: &str, run_id: &str, action: &Step) -> Result<String, String> {
    let phones: Vec<String> = super::desk_features::ready_devices()
        .into_iter()
        .filter(|device| super::features::peer_accepts(device, PACKET_TYPE_AUTOMATIONS))
        .collect();
    if phones.is_empty() {
        return Err("No phone is connected".into());
    }
    let mut packet = NetworkPacket::new(PACKET_TYPE_AUTOMATIONS);
    packet.set("op", "do");
    packet.set("ruleId", rule_id);
    packet.set("runId", run_id);
    packet.set("action", json!(action));
    for phone in &phones {
        super::desk_features::send(phone, &packet);
    }
    Ok(format!(
        "Asked {}: {}",
        phones
            .iter()
            .map(|id| device_name(id))
            .collect::<Vec<_>>()
            .join(", "),
        describe(action, action_kind(&action.kind))
    ))
}

const NOT_APPROVED: &str = "Not approved on this computer yet";

/// Whether rule `id`'s desk actions may run: saved or approved on this
/// computer (or built in), not merely synced from a phone.
fn approved_here(id: &str) -> bool {
    with_shelf(|shelf| {
        (
            shelf.origin.get(id).is_none_or(|origin| origin == DESK),
            false,
        )
    })
    .unwrap_or(false)
}

/// Whether `rule` has desk actions that wait for the user's approval here.
pub(crate) fn needs_approval(rule: &Rule) -> bool {
    !approved_here(&rule.id)
        && rule
            .actions
            .iter()
            .any(|step| action_kind(&step.kind).is_some_and(|kind| kind.side == Side::Desk))
}

/// "Approve": the user lets a rule a phone made act on this computer. Local
/// only; the rule itself is unchanged.
pub(crate) fn approve(id: &str) {
    with_shelf(|shelf| {
        let known = shelf.rules.get(id).is_some_and(|rule| !rule.deleted);
        if known {
            shelf.origin.insert(id.to_string(), DESK.into());
        }
        ((), known)
    });
}

/// A phone's `do`: only an identical action of a rule here that the phone
/// runs, enabled, and approved on this computer.
fn run_requested(device: &str, rule_id: &str, action: &Step) -> Result<String, String> {
    let rule = rules()
        .into_iter()
        .find(|rule| rule.id == rule_id)
        .ok_or("refused")?;
    let known = rule.enabled
        && approved_here(rule_id)
        && rule.run_on == Side::Phone
        && rule.actions.iter().any(|step| step == action)
        && action_kind(&action.kind).is_some_and(|kind| kind.side == Side::Desk);
    if !known {
        return Err("refused".into());
    }
    run_action(action, Some(device))
}

/// The wire's error code for a refusal.
fn code(error: &str) -> &'static str {
    match error {
        "refused" => "refused",
        "gate" => "gate",
        "unsupported" => "unsupported",
        _ => "failed",
    }
}

fn done_packet(run_id: &str, result: Result<(), &str>) -> NetworkPacket {
    let mut packet = NetworkPacket::new(PACKET_TYPE_AUTOMATIONS);
    packet.set("op", "done");
    packet.set("runId", run_id);
    packet.set("ok", result.is_ok());
    if let Err(error) = result {
        packet.set("error", error);
    }
    packet
}

/// One desk action. `gate`: the phone whose permission it needs.
fn run_action(action: &Step, gate: Option<&str>) -> Result<String, String> {
    match action.kind.as_str() {
        "run-command" => {
            if let Some(device) = gate {
                super::desk_features::terminal_gate(device).map_err(|_| "gate".to_string())?;
            }
            let id = action.text("commandId").unwrap_or_default();
            let command = super::config::app_settings()
                .run_commands
                .into_iter()
                .find(|command| command.id == id)
                .ok_or("That saved command was removed")?;
            super::pty::registry::run_command(&command.name, &command.command, gate.unwrap_or(DESK))
                .map(|_| format!("Ran “{}”", command.name))
                .map_err(|_| "The command could not start".to_string())
        }
        "lock-desk" => super::logind::lock()
            .map(|()| "Locked this computer".into())
            .map_err(|_| "This computer could not be locked".into()),
        "keep-awake" => {
            let on = action
                .params
                .get("on")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            super::desk_features::change_settings(|settings| settings.stay_awake = on)
                .map(|()| format!("Keep awake {}", if on { "on" } else { "off" }))
                .map_err(|_| "The setting could not be saved".into())
        }
        "send-clipboard" => {
            if !super::config::app_settings().clipboard_sync {
                return Err("Clipboard sharing is off".into());
            }
            let text = arboard::Clipboard::new()
                .and_then(|mut clipboard| clipboard.get_text())
                .unwrap_or_default();
            let packet = super::glide::clipboard_packet(text).ok_or("The clipboard has no text")?;
            for device in super::desk_features::ready_devices() {
                super::desk_features::send(&device, &packet);
            }
            Ok("Sent the clipboard".into())
        }
        "open-url" => {
            let url = action
                .text("url")
                .filter(|url| web_address(url))
                .ok_or("Only web addresses can be opened")?
                .to_string();
            on_main(move || {
                gtk::gio::AppInfo::launch_default_for_uri(&url, None::<&gtk::gio::AppLaunchContext>)
                    .is_ok()
            })
            .then(|| "Opened the page".to_string())
            .ok_or_else(|| "The page could not be opened".to_string())
        }
        "notify" => {
            let title: String = action
                .text("title")
                .unwrap_or("DeskLink")
                .chars()
                .take(120)
                .collect();
            let body: String = action
                .text("body")
                .unwrap_or_default()
                .chars()
                .take(MAX_PARAM_CHARS)
                .collect();
            on_main(move || {
                use gtk::prelude::ApplicationExt;
                let Some(app) = gtk::gio::Application::default() else {
                    return false;
                };
                let notification = gtk::gio::Notification::new(&title);
                if !body.is_empty() {
                    notification.set_body(Some(&body));
                }
                app.send_notification(Some(&format!("desklink-rule-{}", now_ms())), &notification);
                true
            })
            .then(|| "Showed a notification".to_string())
            .ok_or_else(|| "The notification could not be shown".to_string())
        }
        "pause-media" => {
            let paused = super::mpris::pause_playing();
            Ok(if paused.is_empty() {
                "Nothing was playing".into()
            } else {
                "Paused the music".into()
            })
        }
        _ => Err("unsupported".into()),
    }
}

/// Runs `job` on the GTK thread and waits (bounded) for its answer.
fn on_main(job: impl FnOnce() -> bool + Send + 'static) -> bool {
    let (sender, receiver) = mpsc::sync_channel(1);
    gtk::glib::MainContext::default().invoke(move || {
        let _ = sender.send(job());
    });
    receiver
        .recv_timeout(Duration::from_secs(2))
        .unwrap_or(false)
}

fn record_run(rule_id: &str, run: Run) {
    with_shelf(|shelf| {
        if !shelf.rules.contains_key(rule_id) {
            return ((), false);
        }
        let runs = shelf.history.entry(rule_id.to_string()).or_default();
        runs.push_back(run);
        while runs.len() > HISTORY {
            runs.pop_front();
        }
        ((), true)
    });
}

/// A phone answered a `do`: its result joins that run's lines.
fn record_done(run_id: &str, ok: bool, error: &str) {
    with_shelf(|shelf| {
        let run = shelf
            .history
            .values_mut()
            .flat_map(|runs| runs.iter_mut())
            .find(|run| run.id == run_id);
        match run {
            Some(run) if run.results.len() < MAX_ACTIONS * 2 => {
                run.results.push(if ok {
                    "The phone did it".to_string()
                } else {
                    format!(
                        "The phone refused ({})",
                        error.chars().take(20).collect::<String>()
                    )
                });
                run.ok &= ok;
                ((), true)
            }
            _ => ((), false),
        }
    });
}

/// logind's lock and sleep signals as triggers. The subscriptions live as
/// long as the returned value.
fn subscribe_logind() -> Option<(
    gtk::gio::DBusConnection,
    Vec<gtk::gio::SignalSubscriptionId>,
)> {
    use gtk::gio;
    let bus = gio::bus_get_sync(gio::BusType::System, gio::Cancellable::NONE).ok()?;
    let mut ids = Vec::new();
    ids.push(bus.signal_subscribe(
        Some("org.freedesktop.login1"),
        Some("org.freedesktop.login1.Manager"),
        Some("PrepareForSleep"),
        Some("/org/freedesktop/login1"),
        None,
        gio::DBusSignalFlags::NONE,
        |_, _, _, _, _, parameters| {
            crate::guard("sleep signal", (), || {
                if let Some(sleeping) = parameters.child_value(0).get::<bool>() {
                    fire(Step::new(if sleeping {
                        "desk-sleep"
                    } else {
                        "desk-resume"
                    }));
                }
            })
        },
    ));
    let session = super::logind::display_session_path();
    if let Some(session) = session {
        let last = std::cell::Cell::new(super::logind::locked());
        ids.push(bus.signal_subscribe(
            Some("org.freedesktop.login1"),
            Some("org.freedesktop.DBus.Properties"),
            Some("PropertiesChanged"),
            Some(&session),
            None,
            gio::DBusSignalFlags::NONE,
            move |_, _, _, _, _, parameters| {
                crate::guard("lock signal", (), || {
                    let changed = parameters.child_value(1);
                    let dict = gtk::glib::VariantDict::new(Some(&changed));
                    let locked = dict
                        .lookup_value("LockedHint", None)
                        .and_then(|value| value.get::<bool>());
                    if let Some(locked) = locked {
                        if last.replace(Some(locked)) != Some(locked) {
                            fire(Step::new(if locked {
                                "desk-locked"
                            } else {
                                "desk-unlocked"
                            }));
                        }
                    }
                })
            },
        ));
    }
    Some((bus, ids))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The store is one per process: tests that install it take turns.

    #[test]
    fn every_items_packet_fits_one_wire_message() {
        let big = |index: usize| {
            let mut step = Step::new("notify");
            for key in 0..MAX_PARAMS {
                step = step.with(&format!("k{key}"), "\"".repeat(MAX_PARAM_CHARS / 4));
            }
            let mut rule = rule(step.clone(), vec![step.clone(); 2]);
            rule.id = format!("r{index}");
            serde_json::to_value(rule).unwrap()
        };
        let packets = items_packets((0..40).map(big).collect());
        assert!(!packets.is_empty());
        for packet in &packets {
            assert!(crate::device_links::webrtc::fits_on_wire(packet));
        }
    }
    static STORE_TEST: Mutex<()> = Mutex::new(());

    fn rule(trigger: Step, actions: Vec<Step>) -> Rule {
        Rule {
            id: "r1".into(),
            name: String::new(),
            enabled: true,
            trigger,
            conditions: Vec::new(),
            actions,
            run_on: Side::Desk,
            modified: 10,
            deleted: false,
        }
    }

    #[test]
    fn the_wire_shape_is_the_contract() {
        let rule = rule(
            Step::new("wifi")
                .with("state", "connected")
                .with("ssid", "Home"),
            vec![Step::new("keep-awake").with("on", true)],
        );
        let value = serde_json::to_value(&rule).unwrap();
        assert_eq!(
            value,
            json!({
                "id": "r1", "name": "", "enabled": true,
                "trigger": {"kind": "wifi", "params": {"state": "connected", "ssid": "Home"}},
                "conditions": [], "actions": [{"kind": "keep-awake", "params": {"on": true}}],
                "runOn": "desk", "modified": 10
            })
        );
        assert_eq!(serde_json::from_value::<Rule>(value).unwrap(), rule);
    }

    #[test]
    fn triggers_match_by_kind_and_the_params_the_rule_sets() {
        let wifi = Step::new("wifi")
            .with("state", "connected")
            .with("ssid", "Home");
        let event = Step::new("wifi")
            .with("state", "connected")
            .with("ssid", "home");
        assert!(matches(&wifi, &event), "case-insensitive");
        assert!(!matches(&wifi, &event.clone().with("ssid", "Office")));
        assert!(
            matches(&Step::new("wifi").with("state", "connected"), &event),
            "any network"
        );
        assert!(!matches(&Step::new("charging"), &event));
        let low = Step::new("battery").with("below", 20);
        assert!(matches(&low, &Step::new("battery").with("below", 15)));
        assert!(!matches(&low, &Step::new("battery").with("below", 30)));
        let weekdays = Step::new("time").with("at", "07:30").with("days", "12345");
        let monday = Step::new("time").with("at", "07:30").with("day", "1");
        assert!(matches(&weekdays, &monday));
        assert!(!matches(&weekdays, &monday.clone().with("day", "6")));
        assert!(!matches(&weekdays, &monday.with("at", "07:31")));
        let job = Step::new("command-finished").with("job", "cargo");
        assert!(matches(
            &job,
            &Step::new("command-finished").with("job", "cargo")
        ));
        assert!(!matches(
            &job,
            &Step::new("command-finished").with("job", "Cargo")
        ));
    }

    #[test]
    fn conditions_check_the_world() {
        let world = World {
            connected: true,
            wifi: vec!["Home".into()],
            minute: 23 * 60 + 30,
            locked: Some(false),
        };
        assert!(holds(
            &Step::new("phone-connected").with("connected", true),
            &world
        ));
        assert!(!holds(
            &Step::new("phone-connected").with("connected", false),
            &world
        ));
        assert!(holds(&Step::new("wifi").with("ssid", "HOME"), &world));
        assert!(!holds(&Step::new("wifi").with("ssid", "Office"), &world));
        let night = Step::new("time-between")
            .with("from", "22:00")
            .with("to", "06:00");
        assert!(holds(&night, &world), "wraps midnight");
        let day = Step::new("time-between")
            .with("from", "09:00")
            .with("to", "17:00");
        assert!(!holds(&day, &world));
        assert!(holds(
            &Step::new("desk-locked").with("locked", false),
            &world
        ));
        assert!(!holds(&Step::new("nonsense"), &world));
    }

    #[test]
    fn rules_over_a_bound_are_refused() {
        let ok = rule(Step::new("desk-locked"), vec![Step::new("pause-media")]);
        assert!(valid_rule(&ok));
        let mut long_name = ok.clone();
        long_name.name = "x".repeat(MAX_NAME_CHARS + 1);
        assert!(!valid_rule(&long_name));
        let mut many = ok.clone();
        many.actions = vec![Step::new("pause-media"); MAX_ACTIONS + 1];
        assert!(!valid_rule(&many));
        let mut conditions = ok.clone();
        conditions.conditions = vec![Step::new("desk-locked"); MAX_CONDITIONS + 1];
        assert!(!valid_rule(&conditions));
        let mut params = ok.clone();
        for index in 0..=MAX_PARAMS {
            params.trigger.params.insert(format!("k{index}"), json!(1));
        }
        assert!(!valid_rule(&params));
        let mut nested = ok.clone();
        nested.trigger.params.insert("k".into(), json!({"deep": 1}));
        assert!(!valid_rule(&nested));
        let mut text = ok.clone();
        text.trigger
            .params
            .insert("k".into(), json!("x".repeat(MAX_PARAM_CHARS + 1)));
        assert!(!valid_rule(&text));
        let mut empty = ok.clone();
        empty.actions.clear();
        assert!(!valid_rule(&empty), "a live rule does something");
        empty.deleted = true;
        assert!(valid_rule(&empty), "a tombstone may be empty");
        let mut id = ok;
        id.id = "x".repeat(MAX_ID_CHARS + 1);
        assert!(!valid_rule(&id));
    }

    #[test]
    fn sync_is_last_writer_wins_and_built_ins_take_only_enabled() {
        let mut shelf = Shelf::default();
        let older = rule(Step::new("desk-locked"), vec![Step::new("pause-media")]);
        assert!(merge(&mut shelf, older.clone(), "phone"));
        assert_eq!(shelf.origin["r1"], "phone");
        assert!(
            !merge(&mut shelf, older.clone(), "phone"),
            "equal keeps ours"
        );
        let mut newer = older.clone();
        newer.modified = 20;
        newer.enabled = false;
        assert!(merge(&mut shelf, newer, "phone"));
        assert!(!shelf.rules["r1"].enabled);
        // Built-in: only enabled crosses.
        shelf
            .rules
            .insert(BUILTIN_CALL_PAUSE.into(), builtin_call_pause());
        let mut changed = builtin_call_pause();
        changed.modified = 5;
        changed.enabled = false;
        changed.actions = vec![Step::new("lock-desk")];
        assert!(merge(&mut shelf, changed, "phone"));
        let builtin = &shelf.rules[BUILTIN_CALL_PAUSE];
        assert!(!builtin.enabled);
        assert_eq!(builtin.actions, vec![Step::new("pause-media")]);
        // An index asks for what is missing or newer.
        let index = vec![
            json!({"id": "r1", "modified": 20}),
            json!({"id": "r2", "modified": 1}),
            json!({"id": "r1x", "modified": 1, "deleted": true}),
        ];
        assert_eq!(wanted(&shelf, &index), vec!["r2", "r1x"]);
    }

    #[test]
    fn the_shelf_keeps_at_most_max_rules() {
        let mut shelf = Shelf::default();
        for index in 0..MAX_RULES + 5 {
            let mut item = rule(Step::new("desk-locked"), vec![Step::new("pause-media")]);
            item.id = format!("r{index}");
            item.modified = 100 + index as i64;
            shelf.rules.insert(item.id.clone(), item);
        }
        prune(&mut shelf, now_ms());
        assert_eq!(shelf.rules.len(), MAX_RULES);
        assert!(!shelf.rules.contains_key("r0"), "the oldest went first");
    }

    #[test]
    fn items_packets_stay_under_the_bound() {
        let big = json!({"id": "x", "pad": "y".repeat(15_000)});
        let packets = items_packets(vec![big.clone(), big.clone(), big.clone()]);
        assert_eq!(packets.len(), 2);
        for packet in &packets {
            assert!(packet.serialize().unwrap().len() < MAX_PACKET_BYTES + 1024);
        }
        let too_big = json!({"id": "x", "pad": "y".repeat(MAX_PACKET_BYTES)});
        assert!(items_packets(vec![too_big]).is_empty());
    }

    #[test]
    fn events_and_dos_are_rate_limited_per_phone() {
        let allowed_events = (0..EVENTS_PER_MINUTE + 3)
            .filter(|_| EVENT_BUCKETS.allow("rate-phone"))
            .count();
        assert_eq!(allowed_events, EVENTS_PER_MINUTE as usize);
        assert!(EVENT_BUCKETS.allow("other-phone"));
    }

    #[test]
    fn a_phone_cannot_ask_for_an_action_its_rule_does_not_have() {
        // Without a store every request is refused.
        assert_eq!(
            run_requested("p", "nope", &Step::new("lock-desk")),
            Err("refused".into())
        );
    }

    #[test]
    fn the_editor_saves_checks_and_deletes_rules() {
        let _store = STORE_TEST
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let dir = std::env::temp_dir().join(format!("desklink-rules-{}", uuid::Uuid::new_v4()));
        install(dir.clone());
        assert!(rules().iter().any(|rule| rule.id == BUILTIN_CALL_PAUSE));
        let mut new = rule(
            Step::new("wifi")
                .with("state", "connected")
                .with("ssid", "Home"),
            vec![Step::new("notify").with("title", "Hi")],
        );
        new.id.clear();
        new.modified = 0;
        save_rule(new.clone()).unwrap();
        let saved = rules().into_iter().find(|rule| !rule.is_builtin()).unwrap();
        assert!(saved.modified > 0 && !saved.id.is_empty());
        let mut bad = new.clone();
        bad.actions = vec![Step::new("open-url").with("url", "file:///etc/passwd")];
        assert!(save_rule(bad).is_err());
        let mut no_title = new.clone();
        no_title.actions = vec![Step::new("notify")];
        assert!(save_rule(no_title).is_err(), "a required param is missing");
        assert!(
            save_rule(builtin_call_pause()).is_err(),
            "built-ins are not edited"
        );
        set_enabled(BUILTIN_CALL_PAUSE, false);
        assert!(!builtin_enabled(BUILTIN_CALL_PAUSE));
        delete_rule(&saved.id);
        assert!(rules().iter().all(|rule| rule.id != saved.id));
        // Reloads from the sealed file.
        install(dir.clone());
        assert!(!builtin_enabled(BUILTIN_CALL_PAUSE));
        // A corrupt file is set aside, not fatal.
        std::fs::write(dir.join(FILE), b"garbage").unwrap();
        install(dir.clone());
        assert!(builtin_enabled(BUILTIN_CALL_PAUSE));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_phone_made_rule_acts_here_only_once_approved() {
        let _store = STORE_TEST
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let dir = std::env::temp_dir().join(format!("desklink-approve-{}", uuid::Uuid::new_v4()));
        install(dir.clone());
        let mut synced = rule(
            Step::new("wifi").with("state", "connected"),
            vec![Step::new("lock-desk")],
        );
        synced.id = "from-phone".into();
        synced.enabled = false;
        synced.run_on = Side::Phone;
        synced.modified = now_ms();
        let mut packet = NetworkPacket::new(PACKET_TYPE_AUTOMATIONS);
        packet.set("op", "items");
        packet.set("items", json!([synced]));
        on_packet("phone-a", &packet);
        let stored = || {
            rules()
                .into_iter()
                .find(|rule| rule.id == "from-phone")
                .unwrap()
        };
        assert!(needs_approval(&stored()));
        // Turning it on is not approving it.
        set_enabled("from-phone", true);
        assert!(stored().enabled && needs_approval(&stored()));
        assert_eq!(
            run_requested("phone-a", "from-phone", &Step::new("lock-desk")),
            Err("refused".into()),
            "a synced rule cannot make the phone's request valid"
        );
        approve("from-phone");
        assert!(!needs_approval(&stored()));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn versions_from_the_far_future_are_refused_and_never_overflow() {
        let mut far = rule(Step::new("desk-locked"), vec![Step::new("pause-media")]);
        far.modified = i64::MAX;
        assert!(!valid_rule(&far));
        far.modified = now_ms() + MAX_FUTURE_MS / 2;
        assert!(valid_rule(&far), "a clock a little ahead is fine");
        let mut shelf = Shelf::default();
        let mut max = far.clone();
        max.modified = i64::MAX;
        shelf.rules.insert(max.id.clone(), max);
        with_shelf_test(&mut shelf);
    }

    /// `set_enabled`'s arithmetic on a stored `i64::MAX` (it cannot arrive
    /// by sync any more, but an old file could hold one).
    fn with_shelf_test(shelf: &mut Shelf) {
        let rule = shelf.rules.values_mut().next().unwrap();
        rule.modified = now_ms().max(rule.modified.saturating_add(1));
        assert_eq!(rule.modified, i64::MAX);
    }

    #[test]
    fn web_addresses_only() {
        assert!(web_address("https://example.com/a?b"));
        assert!(!web_address("file:///etc/passwd"));
        assert!(!web_address("https://exa mple.com"));
        assert!(!web_address("javascript:alert(1)"));
        assert_eq!(parse_time("07:30"), Some((7, 30)));
        assert_eq!(parse_time("24:00"), None);
        assert_eq!(parse_time("7:30"), None);
    }

    #[test]
    fn summaries_read_as_when_if_do() {
        let mut rule = rule(
            Step::new("phone-left"),
            vec![
                Step::new("lock-desk"),
                Step::new("keep-awake").with("on", false),
            ],
        );
        rule.conditions = vec![Step::new("desk-locked").with("locked", false)];
        assert_eq!(
            rule.summary(),
            "When the phone leaves · if this computer is locked (off) · do lock this \
             computer, then keep this computer awake (off)"
        );
        rule.trigger = Step::new("teleport");
        assert!(!rule.supported());
        assert!(rule.summary().contains("needs a newer DeskLink"));
    }
}
