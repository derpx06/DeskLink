//! The phone's calls on this computer (contract T1–T4): reading its
//! `desklink.telephony` snapshots, building `desklink.telephony.request`,
//! `tel:` numbers, and pausing this computer's players for a call.

use std::collections::BTreeSet;
use std::sync::Mutex;

use serde_json::Value;

use super::device::{CallState, CallStatus, PhoneCall};
use super::packet::{NetworkPacket, PACKET_TYPE_TELEPHONY_REQUEST};

/// What a request may ask (T2). `message` opens the phone's messaging app
/// on a number (F5 K5).
pub(crate) const ACTIONS: [&str; 8] = [
    "answer", "decline", "end", "silence", "mute", "unmute", "dial", "message",
];

/// More calls than any phone holds at once; the rest are ignored.
const MAX_CALLS: usize = 4;

/// Bidirectional marks a dialer wraps names and numbers in (LRM/RLM, the
/// embeddings and isolates): invisible, but they showed as boxes and broke
/// matching a number.
fn is_bidi_mark(c: char) -> bool {
    matches!(c, '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

fn clean(value: Option<&Value>, max_chars: usize) -> Option<String> {
    let text: String = value?
        .as_str()?
        .chars()
        .filter(|c| !c.is_control() && !is_bidi_mark(*c))
        .take(max_chars)
        .collect();
    let text = text.trim().to_string();
    (!text.is_empty()).then_some(text)
}

fn parse_call(value: &Value) -> Option<PhoneCall> {
    let state = match value.get("state")?.as_str()? {
        "ringing" => CallState::Ringing,
        "dialing" => CallState::Dialing,
        "talking" => CallState::Talking,
        "held" => CallState::Held,
        _ => return None,
    };
    Some(PhoneCall {
        call_id: clean(value.get("callId"), 120)?,
        state,
        contact_name: clean(value.get("contactName"), 120),
        number: clean(value.get("number"), 40),
        started_at: value.get("startedAt").and_then(Value::as_i64),
        incoming: value
            .get("incoming")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

/// A T1 snapshot: `None` when the packet is not one (a missed call, an
/// error), `Some(None)` when the phone has no call.
pub(crate) fn parse_status(packet: &NetworkPacket) -> Option<Option<CallStatus>> {
    let calls: Vec<PhoneCall> = packet
        .body
        .get("calls")?
        .as_array()?
        .iter()
        .filter_map(parse_call)
        .take(MAX_CALLS)
        .collect();
    Some((!calls.is_empty()).then(|| CallStatus {
        calls,
        muted: packet.get_bool("muted").unwrap_or(false),
        silenced: packet.get_bool("silenced").unwrap_or(false),
    }))
}

/// A one-shot missed call: who, when known.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Missed {
    pub(crate) contact_name: Option<String>,
    pub(crate) number: Option<String>,
    /// The caller's uid in the contacts the phone shares (F5 K4).
    pub(crate) contact_uid: Option<String>,
}

pub(crate) fn parse_missed(packet: &NetworkPacket) -> Option<Missed> {
    let missed = packet.body.get("missed")?.as_object()?;
    Some(Missed {
        contact_name: clean(missed.get("contactName"), 120),
        number: clean(missed.get("number"), 40),
        contact_uid: clean(missed.get("contactUid"), 256),
    })
}

/// Each call's `contactUid` (F5 K4), by call id.
pub(crate) fn contact_uids(packet: &NetworkPacket) -> Vec<(String, String)> {
    packet
        .body
        .get("calls")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|call| {
            Some((
                clean(call.get("callId"), 120)?,
                clean(call.get("contactUid"), 256)?,
            ))
        })
        .collect()
}

/// Names the callers `status` has no name for from the phone's shared
/// contacts (`lookup`: uid and number to a name), when the user asked.
pub(crate) fn name_callers(
    status: &mut CallStatus,
    uids: &[(String, String)],
    lookup: impl Fn(Option<&str>, Option<&str>) -> Option<String>,
) {
    for call in &mut status.calls {
        if call.contact_name.is_some() {
            continue;
        }
        let uid = uids
            .iter()
            .find(|(id, _)| *id == call.call_id)
            .map(|(_, uid)| uid.as_str());
        call.contact_name = lookup(uid, call.number.as_deref());
    }
}

/// A refused request, in the user's words.
pub(crate) fn parse_error(packet: &NetworkPacket) -> Option<String> {
    let why = match packet.get_str("error")? {
        "permission" => "it needs the Phone permission (open DeskLink on the phone)",
        "no-call" => "there is no call",
        _ => "the phone could not do it",
    };
    Some(match packet.get_str("action").unwrap_or_default() {
        "dial" => format!("Could not call: {why}"),
        "message" => format!("Could not open messages: {why}"),
        "" => format!("Call: {why}"),
        action => format!("Could not {action} the call: {why}"),
    })
}

/// The request for `action` on the calls the phone last reported. Answer,
/// decline and silence go to the ringing call, end to the live one; dial
/// needs a number.
pub(crate) fn request(
    status: Option<&CallStatus>,
    action: &str,
    number: Option<&str>,
) -> Result<NetworkPacket, String> {
    if !ACTIONS.contains(&action) {
        return Err(format!("\"{action}\" is not a call action"));
    }
    let mut packet = NetworkPacket::new(PACKET_TYPE_TELEPHONY_REQUEST);
    packet.set("action", action);
    match action {
        "answer" | "decline" | "silence" => {
            let call = status
                .and_then(CallStatus::ringing)
                .ok_or("No call is ringing")?;
            packet.set("callId", call.call_id.as_str());
        }
        "end" => {
            let call = status.map(CallStatus::primary).ok_or("There is no call")?;
            packet.set("callId", call.call_id.as_str());
        }
        "mute" | "unmute" => {
            status.ok_or("There is no call")?;
        }
        _ => {
            let number = number.and_then(dial_number).ok_or("Not a phone number")?;
            packet.set("number", number);
        }
    }
    Ok(packet)
}

/// The number in a `tel:` URI or typed text: `+` (first only) and digits, with
/// spaces, dashes, dots and brackets dropped. `None` for anything else, or
/// fewer than three or more than 24 digits. `*` and `#` are refused outright:
/// the phone hands the number to its telephony stack, where `**21*number#`
/// (call forwarding), `**04*…#` (a SIM PIN change) and `*#06#` are not calls.
pub(crate) fn dial_number(text: &str) -> Option<String> {
    let text = text.trim();
    let text = match text.get(..4) {
        Some(scheme) if scheme.eq_ignore_ascii_case("tel:") => &text[4..],
        _ => text,
    };
    let text = text.trim_start_matches('/');
    // Parameters (`;phone-context=…`) and a query are not the number.
    let text = text.split([';', '?']).next().unwrap_or_default();
    let decoded = percent_decode(text)?;
    let mut number = String::new();
    for (index, c) in decoded.chars().enumerate() {
        match c {
            '0'..='9' => number.push(c),
            '+' if index == 0 => number.push(c),
            ' ' | '-' | '.' | '(' | ')' | '\u{a0}' => {}
            _ => return None,
        }
    }
    let digits = number.chars().filter(char::is_ascii_digit).count();
    (3..=24).contains(&digits).then_some(number)
}

fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = text.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Which phones are in a call, and the players DeskLink paused for them
/// (T4): resumed when the last call ends, and only those.
#[derive(Debug, Default)]
pub(crate) struct MediaHold {
    in_call: BTreeSet<String>,
    paused: Vec<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum HoldStep {
    /// The first call began: pause what is playing, then `record` it.
    Pause,
    /// The last call ended: play these again.
    Resume(Vec<String>),
    Nothing,
}

impl MediaHold {
    pub(crate) fn update(&mut self, device_id: &str, in_call: bool) -> HoldStep {
        let before = !self.in_call.is_empty();
        if in_call {
            self.in_call.insert(device_id.to_string());
        } else {
            self.in_call.remove(device_id);
        }
        match (before, !self.in_call.is_empty()) {
            (false, true) => HoldStep::Pause,
            (true, false) => HoldStep::Resume(std::mem::take(&mut self.paused)),
            _ => HoldStep::Nothing,
        }
    }

    pub(crate) fn record(&mut self, paused: Vec<String>) {
        self.paused.extend(paused);
    }
}

static HOLD: Mutex<Option<MediaHold>> = Mutex::new(None);

/// Pauses this computer's players when `device_id`'s first call starts and
/// resumes them after the last one ends. Runs on the desk worker, which also
/// keeps the steps in order.
// ponytail: the desk worker drops a job when 16 are waiting; a dropped
// resume leaves the player paused (Play fixes it). A dedicated queue if seen.
pub(crate) fn hold_media(device_id: &str, in_call: bool) {
    let device_id = device_id.to_string();
    super::desk_features::run_later(move || {
        let step = HOLD
            .lock()
            .map(|mut hold| {
                hold.get_or_insert_with(MediaHold::default)
                    .update(&device_id, in_call)
            })
            .unwrap_or(HoldStep::Nothing);
        match step {
            // "Pause music during calls" (on by default): off pauses nothing,
            // and a resume after it is harmless (nothing was recorded).
            HoldStep::Pause if !pause_media_on_call() => {}
            HoldStep::Pause => {
                let paused = super::mpris::pause_playing();
                if let Ok(mut hold) = HOLD.lock() {
                    hold.get_or_insert_with(MediaHold::default).record(paused);
                }
            }
            HoldStep::Resume(players) => super::mpris::play(&players),
            HoldStep::Nothing => {}
        }
    });
}

/// Preferences: pause the desk's music while the phone is in a call.
fn pause_media_on_call() -> bool {
    super::config::app_settings().pause_media_on_call
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn packet(body: Value) -> NetworkPacket {
        let mut packet = NetworkPacket::new(super::super::packet::PACKET_TYPE_TELEPHONY);
        if let Value::Object(map) = body {
            packet.body.extend(map);
        }
        packet
    }

    #[test]
    fn a_snapshot_is_read_and_an_empty_one_means_no_call() {
        let status = parse_status(&packet(json!({
            "calls": [
                {"callId": "1", "state": "held", "incoming": false, "number": "555"},
                {"callId": "2", "state": "ringing", "incoming": true, "contactName": "Alice"},
                {"callId": "3", "state": "on fire"},
                {"state": "talking"}
            ],
            "muted": true,
        })))
        .unwrap()
        .unwrap();
        assert_eq!(
            status.calls.len(),
            2,
            "unknown states and id-less calls are dropped"
        );
        assert!(status.muted && !status.silenced);
        assert_eq!(
            status.primary().call_id,
            "2",
            "the ringing call comes first"
        );
        assert_eq!(status.primary().caller(), "Alice");
        assert_eq!(parse_status(&packet(json!({"calls": []}))), Some(None));
        assert_eq!(
            parse_status(&packet(json!({"missed": {"callId": "9"}}))),
            None
        );
    }

    #[test]
    fn bidi_marks_are_stripped_from_names_and_numbers() {
        let status = parse_status(&packet(json!({
            "calls": [{
                "callId": "1", "state": "ringing", "incoming": true,
                "contactName": "\u{2068}Mara\u{2069}",
                "number": "\u{202A}+1 555 0100\u{202C}\u{200E}"
            }]
        })))
        .unwrap()
        .unwrap();
        let call = status.primary();
        assert_eq!(call.contact_name.as_deref(), Some("Mara"));
        assert_eq!(call.number.as_deref(), Some("+1 555 0100"));
        let missed = parse_missed(&packet(
            json!({"missed": {"callId": "2", "number": "\u{200F}112"}}),
        ));
        assert_eq!(missed.unwrap().number.as_deref(), Some("112"));
    }

    #[test]
    fn a_private_caller_reads_as_incoming_call() {
        let status = parse_status(&packet(json!({
            "calls": [{"callId": "1", "state": "ringing", "incoming": true}]
        })))
        .unwrap()
        .unwrap();
        assert_eq!(status.primary().caller(), "Incoming call");
    }

    #[test]
    fn the_call_state_machine_targets_the_right_call() {
        let status = parse_status(&packet(json!({"calls": [
            {"callId": "live", "state": "talking", "incoming": true, "startedAt": 5},
            {"callId": "new", "state": "ringing", "incoming": true},
        ]})))
        .unwrap();
        let id = |action| {
            request(status.as_ref(), action, None)
                .map(|packet| packet.get_str("callId").map(str::to_string))
        };
        assert_eq!(id("answer"), Ok(Some("new".into())));
        assert_eq!(id("decline"), Ok(Some("new".into())));
        assert_eq!(id("silence"), Ok(Some("new".into())));
        assert_eq!(
            id("end"),
            Ok(Some("new".into())),
            "end on a ringing call declines it"
        );
        assert_eq!(id("mute"), Ok(None));
        let talking = parse_status(&packet(json!({"calls": [
            {"callId": "live", "state": "talking", "incoming": false},
        ]})))
        .unwrap();
        assert!(request(talking.as_ref(), "answer", None).is_err());
        assert_eq!(
            request(talking.as_ref(), "end", None)
                .unwrap()
                .get_str("callId"),
            Some("live")
        );
        assert!(request(None, "end", None).is_err());
        assert!(request(None, "mute", None).is_err());
        assert!(request(None, "reboot", None).is_err());
        let dial = request(None, "dial", Some("tel:+1-555-0100")).unwrap();
        assert_eq!(dial.packet_type, PACKET_TYPE_TELEPHONY_REQUEST);
        assert_eq!(dial.get_str("action"), Some("dial"));
        assert_eq!(dial.get_str("number"), Some("+15550100"));
        assert!(request(None, "dial", Some("call me")).is_err());
        // MMI/USSD codes are never a call, whatever carries them.
        for mmi in ["**21*+19005551234#", "*#06#", "tel:%2A%2A04%2A1%2A2%2A2%23"] {
            assert!(request(None, "dial", Some(mmi)).is_err(), "{mmi}");
            assert!(request(None, "message", Some(mmi)).is_err(), "{mmi}");
        }
        let message = request(None, "message", Some("+1 555 0100")).unwrap();
        assert_eq!(message.get_str("action"), Some("message"));
        assert_eq!(message.get_str("number"), Some("+15550100"));
        assert!(request(None, "message", None).is_err());
    }

    #[test]
    fn tel_uris_and_typed_numbers_parse() {
        for (input, expected) in [
            ("tel:+1-555-010-0199", Some("+15550100199")),
            ("TEL:555 0100", Some("5550100")),
            ("tel://0800%20123", Some("0800123")),
            ("tel:%2B44 20 7946 0000", Some("+442079460000")),
            ("tel:7042;phone-context=example.com", Some("7042")),
            ("(030) 123.456", Some("030123456")),
            ("*#06#", None),
            ("*31#555123", None),
            ("**21*+19005551234#", None),
            ("tel:%2A%2A21%2A1900555%23", None),
            ("tel:**04*1234*5678*5678#", None),
            ("tel:%23%2306%23", None),
            ("555#0100", None),
            ("+1 555 0100", Some("+15550100")),
            ("112", Some("112")),
            ("12", None),
            ("1+2", None),
            ("tel:1-800-FLOWERS", None),
            ("tel:%zz", None),
            ("hello", None),
            ("", None),
        ] {
            assert_eq!(dial_number(input).as_deref(), expected, "{input}");
        }
    }

    /// F5 K4: a caller without a name gets the shared contact's, by the
    /// phone's uid first, else by number; a name the phone sent stays.
    #[test]
    fn callers_are_named_from_the_shared_contacts() {
        let snapshot = packet(json!({"calls": [
            {"callId": "1", "state": "ringing", "incoming": true, "number": "555 0100", "contactUid": "u1"},
            {"callId": "2", "state": "held", "number": "555 0199"},
            {"callId": "3", "state": "held", "contactName": "Phone's name", "contactUid": "u1"},
        ]}));
        let mut status = parse_status(&snapshot).unwrap().unwrap();
        let uids = contact_uids(&snapshot);
        assert_eq!(
            uids,
            vec![("1".into(), "u1".into()), ("3".into(), "u1".into())]
        );
        name_callers(&mut status, &uids, |uid, number| match (uid, number) {
            (Some("u1"), _) => Some("Mara".into()),
            (None, Some("555 0199")) => Some("Bo".into()),
            _ => None,
        });
        let names: Vec<_> = status
            .calls
            .iter()
            .map(|call| call.contact_name.as_deref())
            .collect();
        assert_eq!(names, vec![Some("Mara"), Some("Bo"), Some("Phone's name")]);
    }

    #[test]
    fn missed_calls_and_refusals_are_read() {
        assert_eq!(
            parse_missed(&packet(
                json!({"missed": {"callId": "1", "number": "555", "at": 1}})
            )),
            Some(Missed {
                contact_name: None,
                number: Some("555".into()),
                contact_uid: None,
            })
        );
        assert_eq!(
            parse_missed(&packet(json!({"missed": {"contactUid": "u7"}})))
                .and_then(|missed| missed.contact_uid)
                .as_deref(),
            Some("u7")
        );
        assert_eq!(
            parse_error(&packet(json!({"error": "permission", "action": "answer"}))).as_deref(),
            Some("Could not answer the call: it needs the Phone permission (open DeskLink on the phone)")
        );
        assert_eq!(parse_error(&packet(json!({"calls": []}))), None);
    }

    #[test]
    fn only_the_players_paused_for_a_call_are_resumed_after_the_last_call() {
        let mut hold = MediaHold::default();
        assert_eq!(hold.update("a", true), HoldStep::Pause);
        hold.record(vec!["org.mpris.MediaPlayer2.spotify".into()]);
        // A second phone's call, and the first's next snapshot: nothing new.
        assert_eq!(hold.update("b", true), HoldStep::Nothing);
        assert_eq!(hold.update("a", true), HoldStep::Nothing);
        assert_eq!(
            hold.update("a", false),
            HoldStep::Nothing,
            "b is still in a call"
        );
        assert_eq!(
            hold.update("b", false),
            HoldStep::Resume(vec!["org.mpris.MediaPlayer2.spotify".into()])
        );
        // Resumed once: a later "no call" resumes nothing again.
        assert_eq!(hold.update("b", false), HoldStep::Nothing);
        assert_eq!(hold.update("a", true), HoldStep::Pause);
        assert_eq!(hold.update("a", false), HoldStep::Resume(Vec::new()));
    }
}
