//! Ask on phone / Ask on computer: one mechanism, both ways
//! (`testdata/asks.json`, shared with the phone).
//!
//! A device asks its peer to turn on a feature only the peer can turn on with
//! `desklink.settings.request {set: {<key>: true}}`. The peer never turns
//! anything on from the packet: it shows a prompt, and only the person's
//! Allow there does. Each side's state rides its `desklink.settings` report
//! (`features`, `declined`, `prompts`).
//!
//! This desk asks the phone ([`ask_phone`], [`phone_ask`]) for the
//! [`TO_PHONE`] keys, and is asked by the phone ([`on_phone_request`],
//! [`incoming`], [`take_incoming`]) for the [`TO_DESK`] keys, which it turns
//! on only from the person's click here (`window::answer_ask`).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use super::packet::{NetworkPacket, PACKET_TYPE_SETTINGS_REQUEST};

/// How long the asking side waits for an answer (`waitSeconds`).
pub(crate) const WAIT: Duration = Duration::from_secs(60);
/// An ask nobody pressed for is sent at most this often (`reaskSeconds`).
pub(crate) const REASK: Duration = Duration::from_secs(300);
/// A repeat of the same ask within this long is dropped (`dedupeSeconds`).
pub(crate) const DEDUPE: Duration = Duration::from_secs(5);
/// How long an ask from a phone stays answerable here: a late Allow still
/// counts, and the phone's state follows the report it gets then.
pub(crate) const KEEP: Duration = Duration::from_secs(600);

/// What this desk may ask the phone to turn on, with the phone's words for it
/// ("<computer> asks to <words>").
pub(crate) const TO_PHONE: [(&str, &str); 8] = [
    ("glide", "use Glide on this phone"),
    ("contacts", "see your contacts"),
    ("notifications", "show your notifications"),
    ("calls", "show and answer your calls"),
    ("media", "show and control your music"),
    ("photos", "get your new photos and screenshots"),
    ("dnd", "share Do Not Disturb"),
    ("clipboard", "share the clipboard"),
];

/// What the phone may ask this desk to turn on, and the words of the prompt
/// here ("<phone> asks to <words>").
pub(crate) const TO_DESK: [(&str, &str); 7] = [
    ("terminalAccess", "open terminals on this computer"),
    ("unlockNearby", "unlock this computer"),
    ("galleryAccepted", "send photos and screenshots here"),
    ("keepAwakeAllowed", "keep this computer awake"),
    ("dndSync", "share Do Not Disturb"),
    ("glide", "use Glide with this computer"),
    ("control", "see and control this screen"),
];

fn phrase(table: &[(&'static str, &'static str)], key: &str) -> Option<&'static str> {
    table
        .iter()
        .find(|(known, _)| *known == key)
        .map(|(_, words)| *words)
}

/// The static key for `key`, if it is one of `table`'s.
fn known(table: &[(&'static str, &'static str)], key: &str) -> Option<&'static str> {
    table
        .iter()
        .find(|(known, _)| *known == key)
        .map(|(known, _)| *known)
}

/// Where an ask stands, on the asking side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AskState {
    /// The prompt is up there; nobody has answered yet.
    Waiting,
    /// The person tapped Not now there.
    Declined,
    /// Nobody answered within [`WAIT`].
    TimedOut,
    /// The ask could not be sent (no ready link); it goes once the link is.
    Undelivered,
    /// The other side's notifications cannot show the prompt: it waits in
    /// its app.
    Blocked,
}

/// What the person is told about an ask to `device`, in one line.
pub(crate) fn words(state: AskState, device: &str) -> String {
    match state {
        AskState::Waiting => format!("Waiting for {device}: tap Allow there"),
        AskState::Declined => format!("{device} said not now"),
        AskState::TimedOut => format!("{device} didn't answer"),
        AskState::Undelivered => format!("Couldn't reach {device}"),
        AskState::Blocked => {
            format!("Notifications are off on {device}: open DeskLink there to allow")
        }
    }
}

/// The button that asks: "Ask on phone", or "Ask again" once asked.
pub(crate) fn ask_label(state: Option<AskState>) -> &'static str {
    if state.is_some() {
        "Ask again"
    } else {
        "Ask on phone"
    }
}

/// The prompt here for an ask from `phone`: "<phone> asks to <words>".
pub(crate) fn prompt(phone: &str, key: &str) -> Option<String> {
    phrase(&TO_DESK, key).map(|words| format!("{phone} asks to {words}"))
}

/// The ask itself: `desklink.settings.request {set: {key: true}}`.
pub(crate) fn request(key: &str) -> NetworkPacket {
    let mut packet = NetworkPacket::new(PACKET_TYPE_SETTINGS_REQUEST);
    let mut set = serde_json::Map::new();
    set.insert(key.into(), true.into());
    packet.set("set", serde_json::Value::Object(set));
    packet
}

// ---- Asking the phone --------------------------------------------------------

/// One ask this desk made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Outgoing {
    pub at: Instant,
    pub declined: bool,
    pub delivered: bool,
    /// Already sent again once on a ready link after being undelivered.
    pub retried: bool,
}

impl Outgoing {
    fn new(at: Instant, delivered: bool) -> Self {
        Self {
            at,
            declined: false,
            delivered,
            retried: false,
        }
    }
}

/// What the phone's last `desklink.settings` report said.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct PhoneReport {
    pub features: HashMap<String, bool>,
    /// Its notifications can show the prompt.
    pub prompts: bool,
}

/// The asks this run made to each phone, by `(device, key)`.
static ASKS: Mutex<Option<HashMap<(String, String), Outgoing>>> = Mutex::new(None);
/// Each phone's last report.
static REPORTS: Mutex<Option<HashMap<String, PhoneReport>>> = Mutex::new(None);

fn with_asks<T>(f: impl FnOnce(&mut HashMap<(String, String), Outgoing>) -> T) -> T {
    let mut guard = ASKS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    f(guard.get_or_insert_with(HashMap::new))
}

fn with_reports<T>(f: impl FnOnce(&mut HashMap<String, PhoneReport>) -> T) -> T {
    let mut guard = REPORTS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    f(guard.get_or_insert_with(HashMap::new))
}

/// Where `ask` stands (tested). `on`: the phone reports the feature on, which
/// ends the ask whatever came before (an Allow after the timeout counts).
pub(crate) fn state_of(
    ask: Option<Outgoing>,
    on: Option<bool>,
    prompts: bool,
    now: Instant,
) -> Option<AskState> {
    let ask = ask?;
    Some(if on == Some(true) {
        return None;
    } else if ask.declined {
        AskState::Declined
    } else if !ask.delivered {
        AskState::Undelivered
    } else if !prompts {
        // Its notifications cannot show the request: it waits in its app,
        // which this keeps saying after the wait too.
        AskState::Blocked
    } else if now.saturating_duration_since(ask.at) >= WAIT {
        AskState::TimedOut
    } else {
        AskState::Waiting
    })
}

/// Whether to send an ask now (tested): never while one waits for the
/// person, never for what is on; a press of the button any other time; an
/// automatic one at most every [`REASK`].
pub(crate) fn may_ask(
    ask: Option<Outgoing>,
    on: Option<bool>,
    prompts: bool,
    manual: bool,
    now: Instant,
) -> bool {
    if on == Some(true) {
        return false;
    }
    match ask {
        None => true,
        Some(previous) => {
            if state_of(Some(previous), on, prompts, now) == Some(AskState::Waiting) {
                return false;
            }
            manual || now.saturating_duration_since(previous.at) >= REASK
        }
    }
}

/// Whether the phone has `key` on: its report, or for Glide its own
/// `shareinputdevices` report. `None`: it has not said (an older phone).
pub(crate) fn phone_on(device_id: &str, key: &str) -> Option<bool> {
    if key == "glide" {
        if let Some(allowed) = super::glide::phone_allows(device_id) {
            return Some(allowed);
        }
    }
    with_reports(|reports| {
        reports
            .get(device_id)
            .and_then(|report| report.features.get(key).copied())
    })
}

/// Whether the phone sends the ask report at all (a build with Ask on phone).
pub(crate) fn phone_reports(device_id: &str) -> bool {
    with_reports(|reports| reports.contains_key(device_id))
}

fn phone_prompts(device_id: &str) -> bool {
    with_reports(|reports| reports.get(device_id).is_none_or(|report| report.prompts))
}

/// Where the last ask for `key` to `device_id` stands.
pub(crate) fn phone_ask(device_id: &str, key: &str) -> Option<AskState> {
    let ask = with_asks(|asks| asks.get(&(device_id.to_string(), key.to_string())).copied());
    state_of(
        ask,
        phone_on(device_id, key),
        phone_prompts(device_id),
        Instant::now(),
    )
}

/// Asks `device_id` to turn on `key` (a [`TO_PHONE`] key). `manual`: the
/// person pressed the button (else the rate limit holds it to one per
/// [`REASK`]). Whether an ask went out or was queued for the link.
pub(crate) fn ask_phone(device_id: &str, key: &str, manual: bool) -> bool {
    let Some(key) = known(&TO_PHONE, key) else {
        return false;
    };
    let now = Instant::now();
    let id = (device_id.to_string(), key.to_string());
    let previous = with_asks(|asks| asks.get(&id).copied());
    if !may_ask(
        previous,
        phone_on(device_id, key),
        phone_prompts(device_id),
        manual,
        now,
    ) {
        return false;
    }
    let delivered = super::desk_features::try_send(device_id, &request(key));
    debug_log!("[Asks] asked {device_id} for {key} (delivered: {delivered})");
    with_asks(|asks| {
        if asks.len() < 512 || asks.contains_key(&id) {
            asks.insert(id, Outgoing::new(now, delivered));
        }
    });
    true
}

/// The phone's link is ready: an ask that could not be sent goes now, once.
pub(crate) fn on_ready(device_id: &str) {
    let due: Vec<String> = with_asks(|asks| {
        asks.iter_mut()
            .filter(|((device, _), ask)| device == device_id && !ask.delivered && !ask.retried)
            .map(|((_, key), ask)| {
                ask.retried = true;
                key.clone()
            })
            .collect()
    });
    for key in due {
        let delivered = super::desk_features::try_send(device_id, &request(&key));
        with_asks(|asks| {
            if let Some(ask) = asks.get_mut(&(device_id.to_string(), key)) {
                ask.delivered = delivered;
                // The wait starts when the prompt can be on the phone.
                ask.at = Instant::now();
            }
        });
    }
}

/// The phone tapped Not now on `key` (its report's `declined`, or Glide's
/// `reason: "declined"`).
pub(crate) fn note_declined(device_id: &str, key: &str) {
    with_asks(|asks| {
        let entry = asks
            .entry((device_id.to_string(), key.to_string()))
            .or_insert_with(|| Outgoing::new(Instant::now(), true));
        entry.declined = true;
        entry.delivered = true;
    });
}

/// The phone has `key` on now: its ask is over.
pub(crate) fn note_on(device_id: &str, key: &str) {
    with_asks(|asks| {
        asks.remove(&(device_id.to_string(), key.to_string()));
    });
}

/// Reads a phone's `desklink.settings` report (tested): its features, the
/// keys it just declined, whether its notifications show prompts. `None`
/// for a body without `features` (not an ask report).
pub(crate) fn parse_report(packet: &NetworkPacket) -> Option<(PhoneReport, Vec<String>)> {
    let features = packet.body.get("features")?.as_object()?;
    let features = TO_PHONE
        .iter()
        .filter_map(|(key, _)| {
            features
                .get(*key)
                .and_then(serde_json::Value::as_bool)
                .map(|on| (key.to_string(), on))
        })
        .collect();
    let declined = packet
        .body
        .get("declined")
        .and_then(serde_json::Value::as_array)
        .map(|keys| {
            keys.iter()
                .filter_map(serde_json::Value::as_str)
                .filter_map(|key| known(&TO_PHONE, key))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let prompts = packet
        .body
        .get("prompts")
        .and_then(serde_json::Value::as_str)
        != Some("off");
    Some((PhoneReport { features, prompts }, declined))
}

/// The phone's `desklink.settings` report: a state report, never an
/// instruction (nothing here is turned on by it).
pub(crate) fn on_phone_report(device_id: &str, packet: &NetworkPacket) {
    let Some((report, declined)) = parse_report(packet) else {
        return;
    };
    for (key, on) in &report.features {
        if *on {
            note_on(device_id, key);
        }
    }
    for key in &declined {
        note_declined(device_id, key);
    }
    with_reports(|reports| {
        if reports.len() < 64 || reports.contains_key(device_id) {
            reports.insert(device_id.to_string(), report);
        }
    });
}

// ---- Asked by the phone ----------------------------------------------------------

/// An ask from a phone, waiting for the person here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Incoming {
    pub device: String,
    pub key: &'static str,
    pub at: Instant,
}

static INCOMING: Mutex<Vec<Incoming>> = Mutex::new(Vec::new());

fn with_incoming<T>(f: impl FnOnce(&mut Vec<Incoming>) -> T) -> T {
    let mut guard = INCOMING
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    f(&mut guard)
}

/// The [`TO_DESK`] keys a `desklink.settings.request` asks for (tested):
/// only `true`, only known keys. Settable keys are the request's other,
/// older meaning and are applied by `desk_features` as before.
pub(crate) fn asked_keys(packet: &NetworkPacket) -> Vec<&'static str> {
    let Some(set) = packet
        .body
        .get("set")
        .and_then(serde_json::Value::as_object)
    else {
        return Vec::new();
    };
    set.iter()
        .filter(|(_, value)| value.as_bool() == Some(true))
        .filter_map(|(key, _)| known(&TO_DESK, key))
        .collect()
}

/// Records an ask (tested): one per key per phone, a repeat replaces it
/// (fresh time), one within [`DEDUPE`] is dropped. Whether it is new or
/// renewed (the prompt is shown again).
pub(crate) fn add_incoming(
    pending: &mut Vec<Incoming>,
    device: &str,
    key: &'static str,
    now: Instant,
) -> bool {
    pending.retain(|ask| now.saturating_duration_since(ask.at) < KEEP);
    let full = pending.len() >= 64;
    match pending
        .iter_mut()
        .find(|ask| ask.device == device && ask.key == key)
    {
        Some(ask) if now.saturating_duration_since(ask.at) < DEDUPE => false,
        Some(ask) => {
            ask.at = now;
            true
        }
        None if !full => {
            pending.push(Incoming {
                device: device.to_string(),
                key,
                at: now,
            });
            true
        }
        None => false,
    }
}

/// The phone's asks in a `desklink.settings.request`. Nothing is turned on:
/// the person answers here (a notification, the window, the tray).
pub(crate) fn on_phone_request(device_id: &str, packet: &NetworkPacket) {
    let now = Instant::now();
    for key in asked_keys(packet) {
        if with_incoming(|pending| add_incoming(pending, device_id, key, now)) {
            debug_log!("[Asks] {device_id} asks for {key}");
            // A renewed ask is shown again: the window and the tray re-read
            // the list, the notification is posted under the same id.
            if let Ok(mut told) = NOTIFIED.lock() {
                told.retain(|(device, known)| !(device == device_id && *known == key));
            }
        }
    }
}

/// The phone's asks waiting here, oldest first.
pub(crate) fn incoming() -> Vec<Incoming> {
    let now = Instant::now();
    with_incoming(|pending| {
        pending.retain(|ask| now.saturating_duration_since(ask.at) < KEEP);
        pending.clone()
    })
}

/// The person answered: the ask is gone. Whether it was waiting (only a
/// waiting ask may turn anything on).
pub(crate) fn take_incoming(device_id: &str, key: &str) -> Option<&'static str> {
    let now = Instant::now();
    with_incoming(|pending| {
        let index = pending.iter().position(|ask| {
            ask.device == device_id
                && ask.key == key
                && now.saturating_duration_since(ask.at) < KEEP
        })?;
        Some(pending.remove(index).key)
    })
}

/// Asks already posted as a notification (so each is posted once per ask).
static NOTIFIED: Mutex<Vec<(String, &'static str)>> = Mutex::new(Vec::new());

/// The asks to post as notifications now, and the ids to withdraw (answered
/// or expired) (tested through [`notice_changes`]).
pub(crate) fn notices() -> (Vec<Incoming>, Vec<String>) {
    let pending = incoming();
    let mut told = NOTIFIED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    notice_changes(&pending, &mut told)
}

pub(crate) fn notice_changes(
    pending: &[Incoming],
    told: &mut Vec<(String, &'static str)>,
) -> (Vec<Incoming>, Vec<String>) {
    let gone: Vec<String> = told
        .iter()
        .filter(|(device, key)| {
            !pending
                .iter()
                .any(|ask| &ask.device == device && ask.key == *key)
        })
        .map(|(device, key)| notice_id(device, key))
        .collect();
    told.retain(|(device, key)| {
        pending
            .iter()
            .any(|ask| &ask.device == device && ask.key == *key)
    });
    let new: Vec<Incoming> = pending
        .iter()
        .filter(|ask| {
            !told
                .iter()
                .any(|(device, key)| device == &ask.device && *key == ask.key)
        })
        .cloned()
        .collect();
    told.extend(new.iter().map(|ask| (ask.device.clone(), ask.key)));
    (new, gone)
}

/// The notification id of an ask.
pub(crate) fn notice_id(device: &str, key: &str) -> String {
    format!("ask-{device}-{key}")
}

/// Whether this desktop shows a notification's buttons (tested): a Flatpak
/// goes through the Notification portal, GNOME Shell takes GNotification
/// itself (`org.gtk.Notifications`), anything else needs a freedesktop
/// server that says it has `actions`. No server at all: no.
pub(crate) fn buttons_shown(
    flatpak: bool,
    gtk_notifications: bool,
    caps: Option<&[String]>,
) -> bool {
    flatpak || gtk_notifications || caps.is_some_and(|caps| caps.iter().any(|cap| cap == "actions"))
}

/// 0 unknown (counted as yes), 1 yes, 2 no. Refreshed by the window's tick.
static BUTTONS: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

/// Whether prompts here show their Allow button (the desk's `prompts`).
pub(crate) fn prompts_shown() -> bool {
    BUTTONS.load(std::sync::atomic::Ordering::Relaxed) != 2
}

/// Asks the session bus who shows notifications, and with what. Main loop,
/// at most once a minute; a changed answer is reported to the phones.
pub(crate) fn probe_buttons(bus: &gtk::gio::DBusConnection) {
    use gtk::gio;
    use gtk::glib::prelude::*;
    static LAST: Mutex<Option<Instant>> = Mutex::new(None);
    {
        let mut last = LAST.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if last.is_some_and(|at| at.elapsed() < Duration::from_secs(60)) {
            return;
        }
        *last = Some(Instant::now());
    }
    let has_owner = |name: &str| {
        bus.call_sync(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "NameHasOwner",
            Some(&(name,).to_variant()),
            None,
            gio::DBusCallFlags::NONE,
            500,
            gio::Cancellable::NONE,
        )
        .ok()
        .and_then(|reply| reply.get::<(bool,)>())
        .is_some_and(|(owned,)| owned)
    };
    let flatpak = std::path::Path::new("/.flatpak-info").exists();
    let gtk_notifications = has_owner("org.gtk.Notifications");
    let caps: Option<Vec<String>> = has_owner("org.freedesktop.Notifications")
        .then(|| {
            bus.call_sync(
                Some("org.freedesktop.Notifications"),
                "/org/freedesktop/Notifications",
                "org.freedesktop.Notifications",
                "GetCapabilities",
                None,
                None,
                gio::DBusCallFlags::NONE,
                500,
                gio::Cancellable::NONE,
            )
            .ok()
            .and_then(|reply| reply.get::<(Vec<String>,)>())
            .map(|(caps,)| caps)
        })
        .flatten();
    let shown = buttons_shown(flatpak, gtk_notifications, caps.as_deref());
    let value = if shown { 1 } else { 2 };
    if BUTTONS.swap(value, std::sync::atomic::Ordering::Relaxed) != value {
        super::desk_features::push_settings();
    }
}

/// The desk's part of its own `desklink.settings` report: each [`TO_DESK`]
/// key on or off for `device_id`, and whether its prompts show buttons.
pub(crate) fn desk_report(
    settings: &super::config::AppSettings,
    device_id: &str,
) -> serde_json::Value {
    let features: serde_json::Map<String, serde_json::Value> = TO_DESK
        .iter()
        .map(|(key, _)| {
            let on = match *key {
                "terminalAccess" => super::config::terminal_access_allowed(device_id),
                "unlockNearby" => settings.unlocks_from(device_id),
                "galleryAccepted" => settings.accepts_gallery_from(device_id),
                "keepAwakeAllowed" => settings.lets_keep_awake(device_id),
                "dndSync" => settings.shares_dnd_with(device_id),
                "glide" => settings
                    .glide
                    .as_ref()
                    .is_some_and(|glide| glide.device == device_id),
                "control" => super::config::remote_desktop_token(device_id).is_some(),
                _ => false,
            };
            (key.to_string(), on.into())
        })
        .collect();
    serde_json::Value::Object(features)
}

/// Forgets what was asked of and by `device_id`, and what it reported.
pub(crate) fn forget(device_id: &str) {
    with_asks(|asks| asks.retain(|(device, _), _| device != device_id));
    with_reports(|reports| {
        reports.remove(device_id);
    });
    with_incoming(|pending| pending.retain(|ask| ask.device != device_id));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> serde_json::Value {
        serde_json::from_str(include_str!("../../testdata/asks.json")).unwrap()
    }

    fn table(value: &serde_json::Value) -> Vec<(String, String)> {
        value
            .as_object()
            .unwrap()
            .iter()
            .map(|(key, words)| (key.clone(), words.as_str().unwrap().to_string()))
            .collect()
    }

    fn own(table: &[(&str, &str)]) -> Vec<(String, String)> {
        let mut out: Vec<_> = table
            .iter()
            .map(|(key, words)| (key.to_string(), words.to_string()))
            .collect();
        out.sort();
        out
    }

    /// The keys, the words and the times are the phone's (shared fixture).
    #[test]
    fn the_tables_words_and_times_are_the_shared_ones() {
        let f = fixture();
        let mut to_phone = table(&f["toPhone"]);
        to_phone.sort();
        assert_eq!(own(&TO_PHONE), to_phone);
        let mut to_desk = table(&f["toDesk"]);
        to_desk.sort();
        assert_eq!(own(&TO_DESK), to_desk);
        assert_eq!(WAIT.as_secs(), f["waitSeconds"].as_u64().unwrap());
        assert_eq!(REASK.as_secs(), f["reaskSeconds"].as_u64().unwrap());
        assert_eq!(DEDUPE.as_secs(), f["dedupeSeconds"].as_u64().unwrap());
        let w = &f["words"];
        let fill = |key: &str| w[key].as_str().unwrap().replace("{device}", "Pixel");
        for (state, key) in [
            (AskState::Waiting, "waiting"),
            (AskState::Declined, "declined"),
            (AskState::TimedOut, "timedOut"),
            (AskState::Undelivered, "undelivered"),
            (AskState::Blocked, "blocked"),
        ] {
            assert_eq!(words(state, "Pixel"), fill(key));
        }
        assert_eq!(ask_label(None), w["askOnPhone"].as_str().unwrap());
        assert_eq!(
            ask_label(Some(AskState::Declined)),
            w["askAgain"].as_str().unwrap()
        );
        assert_eq!(
            prompt("Pixel", "terminalAccess").unwrap(),
            w["prompt"]
                .as_str()
                .unwrap()
                .replace("{device}", "Pixel")
                .replace("{feature}", "open terminals on this computer")
        );
        // The request is the fixture's.
        let ask = request("contacts");
        assert_eq!(
            serde_json::Value::Object(ask.body.clone()),
            f["examples"]["ask"]
        );
        // What the phone may ask is never a key the phone sets directly.
        let wire: serde_json::Value =
            serde_json::from_str(include_str!("../../testdata/wire_values.json")).unwrap();
        for settable in wire["settings"]["settable"].as_array().unwrap() {
            assert!(
                phrase(&TO_DESK, settable.as_str().unwrap()).is_none(),
                "{settable} is settable"
            );
        }
    }

    #[test]
    fn an_ask_waits_then_ends_clearly() {
        let at = Instant::now();
        let ask = Some(Outgoing::new(at, true));
        assert_eq!(state_of(None, Some(false), true, at), None, "never asked");
        assert_eq!(
            state_of(ask, Some(false), true, at),
            Some(AskState::Waiting)
        );
        assert_eq!(
            state_of(ask, None, true, at + WAIT - Duration::from_millis(1)),
            Some(AskState::Waiting)
        );
        assert_eq!(
            state_of(ask, Some(false), true, at + WAIT),
            Some(AskState::TimedOut)
        );
        assert_eq!(state_of(ask, Some(true), true, at), None, "allowed");
        assert_eq!(
            state_of(ask, Some(true), true, at + WAIT * 3),
            None,
            "an Allow after the timeout still counts"
        );
        let mut declined = Outgoing::new(at, true);
        declined.declined = true;
        assert_eq!(
            state_of(Some(declined), Some(false), true, at),
            Some(AskState::Declined)
        );
        assert_eq!(
            state_of(Some(Outgoing::new(at, false)), None, true, at),
            Some(AskState::Undelivered)
        );
        assert_eq!(
            state_of(ask, Some(false), false, at),
            Some(AskState::Blocked),
            "its notifications are off: open DeskLink there"
        );
        assert_eq!(
            state_of(ask, Some(false), false, at + WAIT),
            Some(AskState::Blocked),
            "and still after the wait"
        );
    }

    #[test]
    fn asks_are_rate_limited() {
        let at = Instant::now();
        assert!(may_ask(None, Some(false), true, false, at));
        assert!(!may_ask(None, Some(true), true, true, at), "it is on");
        let ask = Some(Outgoing::new(at, true));
        assert!(
            !may_ask(ask, Some(false), true, true, at + Duration::from_secs(1)),
            "one pending per feature"
        );
        let later = at + WAIT;
        assert!(may_ask(ask, Some(false), true, true, later), "Ask again");
        assert!(
            !may_ask(ask, Some(false), true, false, later),
            "no automatic re-ask within five minutes"
        );
        assert!(may_ask(ask, Some(false), true, false, at + REASK));
        // Undelivered or blocked: the button asks again at once.
        assert!(may_ask(
            Some(Outgoing::new(at, false)),
            None,
            true,
            true,
            at
        ));
        assert!(may_ask(ask, Some(false), false, true, at));
    }

    #[test]
    fn an_undelivered_ask_goes_once_the_link_is_ready() {
        let phone = format!(
            "{}asks-ready",
            super::super::desk_features::TEST_OUTBOX_PREFIX
        );
        with_asks(|asks| {
            asks.insert(
                (phone.clone(), "contacts".into()),
                Outgoing::new(Instant::now(), false),
            );
        });
        assert_eq!(phone_ask(&phone, "contacts"), Some(AskState::Undelivered));
        on_ready(&phone);
        assert_eq!(phone_ask(&phone, "contacts"), Some(AskState::Waiting));
        let sent = super::super::desk_features::test_sent(&phone);
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].body["set"]["contacts"], true);
        on_ready(&phone);
        assert_eq!(
            super::super::desk_features::test_sent(&phone).len(),
            1,
            "once"
        );
        forget(&phone);
    }

    #[test]
    fn the_phones_report_ends_or_declines_an_ask() {
        let phone = format!(
            "{}asks-report",
            super::super::desk_features::TEST_OUTBOX_PREFIX
        );
        let f = fixture();
        let packet = |body: &serde_json::Value| {
            let mut packet = NetworkPacket::new(super::super::packet::PACKET_TYPE_SETTINGS);
            for (key, value) in body.as_object().unwrap() {
                packet.body.insert(key.clone(), value.clone());
            }
            packet
        };
        on_phone_report(&phone, &packet(&f["examples"]["report"]));
        assert_eq!(phone_on(&phone, "contacts"), Some(false));
        assert!(ask_phone(&phone, "contacts", true));
        assert_eq!(phone_ask(&phone, "contacts"), Some(AskState::Waiting));
        assert!(!ask_phone(&phone, "contacts", true), "waiting: not again");
        on_phone_report(&phone, &packet(&f["examples"]["declinedReport"]));
        assert_eq!(phone_ask(&phone, "contacts"), Some(AskState::Declined));
        assert!(ask_phone(&phone, "contacts", true), "Ask again");
        on_phone_report(&phone, &packet(&f["examples"]["blockedReport"]));
        assert_eq!(phone_ask(&phone, "contacts"), Some(AskState::Blocked));
        let mut on = f["examples"]["report"].clone();
        on["features"]["contacts"] = true.into();
        on_phone_report(&phone, &packet(&on));
        assert_eq!(phone_ask(&phone, "contacts"), None, "Allow ends it");
        assert!(
            !ask_phone(&phone, "nonsense", true),
            "only the table's keys"
        );
        forget(&phone);
        assert!(!phone_reports(&phone));
    }

    #[test]
    fn the_phones_asks_are_kept_once_and_never_settable_keys() {
        let mut packet = NetworkPacket::new(PACKET_TYPE_SETTINGS_REQUEST);
        packet.set(
            "set",
            serde_json::json!({"terminalAccess": true, "clipboard": true, "control": false, "x": true}),
        );
        assert_eq!(asked_keys(&packet), vec!["terminalAccess"]);

        let mut pending = Vec::new();
        let at = Instant::now();
        assert!(add_incoming(&mut pending, "p", "terminalAccess", at));
        assert!(
            !add_incoming(
                &mut pending,
                "p",
                "terminalAccess",
                at + Duration::from_secs(1)
            ),
            "a repeat within seconds is dropped"
        );
        assert!(
            add_incoming(&mut pending, "p", "terminalAccess", at + DEDUPE),
            "a later one is shown again"
        );
        assert_eq!(pending.len(), 1, "replaced, never stacked");
        assert!(add_incoming(&mut pending, "p", "control", at + DEDUPE));
        assert_eq!(pending.len(), 2);
        assert!(add_incoming(
            &mut pending,
            "p",
            "control",
            at + DEDUPE + KEEP
        ));
        assert_eq!(pending.len(), 1, "an old ask expires");

        // Each is posted once, and withdrawn when answered.
        let mut told = Vec::new();
        let first = vec![Incoming {
            device: "p".into(),
            key: "control",
            at,
        }];
        let (new, gone) = notice_changes(&first, &mut told);
        assert_eq!(new.len(), 1);
        assert!(gone.is_empty());
        let (new, _) = notice_changes(&first, &mut told);
        assert!(new.is_empty(), "posted once");
        let (_, gone) = notice_changes(&[], &mut told);
        assert_eq!(gone, vec![notice_id("p", "control")]);
    }

    /// The Allow button needs a server that shows it; else the ask waits in
    /// the window, the tray and Find anything, and the phone is told.
    #[test]
    fn prompts_show_buttons_only_where_the_desktop_has_them() {
        let caps = |list: &[&str]| list.iter().map(|cap| cap.to_string()).collect::<Vec<_>>();
        assert!(buttons_shown(true, false, None), "Flatpak: the portal");
        assert!(buttons_shown(false, true, None), "GNOME Shell");
        assert!(buttons_shown(
            false,
            false,
            Some(&caps(&["body", "actions"]))
        ));
        assert!(
            !buttons_shown(false, false, Some(&caps(&["body"]))),
            "no actions"
        );
        assert!(!buttons_shown(false, false, None), "no server");
    }
}
