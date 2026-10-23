//! Insert from the phone (F3), desk side.
//!
//! The desk asks with `capture-request {captureKind, target}` (the capture id
//! is the message's `remoteSessionId`). The phone answers `capture-result`:
//! text for `text` and `qr`, or a `mimeType` whose file then follows as an
//! ordinary transfer whose offer carries the `captureId` (and `clipboard:
//! true` for the clipboard). Either side ends one with `capture-cancelled`.
//! One open capture per phone; a new request replaces it.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use super::remote_session::{ControlKind, ControlMessage};
use crate::device_links::daemon::handle::DaemonEvent;

/// What the phone can capture.
pub(crate) const KINDS: [&str; 4] = ["photo", "scan", "text", "qr"];
/// The phone gives up after 120 s; the desk forgets a capture a little
/// after that, in case its cancellation never arrived.
const FORGET_AFTER: Duration = Duration::from_secs(150);
/// Notices kept for a window that is not there.
const MAX_NOTICES: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Capture {
    pub(crate) id: String,
    pub(crate) kind: String,
    pub(crate) target: String,
    /// The result said a file follows.
    pub(crate) receiving: bool,
    /// The phone's name, for saying what arrived.
    phone: String,
    started: Instant,
    sequence: i64,
}

static SESSIONS: Mutex<Option<HashMap<String, Capture>>> = Mutex::new(None);
static NOTICES: Mutex<Vec<DaemonEvent>> = Mutex::new(Vec::new());

fn with<R>(run: impl FnOnce(&mut HashMap<String, Capture>) -> R) -> Option<R> {
    let mut forgotten = 0;
    let result = SESSIONS.lock().ok().map(|mut sessions| {
        let sessions = sessions.get_or_insert_with(HashMap::new);
        let before = sessions.len();
        sessions.retain(|_, capture| capture.started.elapsed() < FORGET_AFTER);
        forgotten = before - sessions.len();
        run(sessions)
    });
    // A capture nobody answered ends with a word, not by vanishing.
    for _ in 0..forgotten {
        notify(DaemonEvent::CaptureNotice {
            device_id: String::new(),
            message: "The phone did not answer in time".to_string(),
        });
    }
    result
}

/// The phone's open capture, for the window.
pub(crate) fn status(device_id: &str) -> Option<Capture> {
    with(|sessions| sessions.get(device_id).cloned()).flatten()
}

/// Builds a message: kind, capture id, sequence.
pub(crate) type Build<'a> = &'a dyn Fn(ControlKind, &str, i64) -> ControlMessage;

/// Asks the phone for a capture; a capture still open is replaced (the
/// phone cancels it). `target` is "clipboard" or "file".
pub(crate) fn begin(device_id: &str, kind: &str, target: &str, build: Build) -> ControlMessage {
    let id = uuid::Uuid::new_v4().to_string();
    let target = if target == "file" {
        "file"
    } else {
        "clipboard"
    };
    let mut request = build(ControlKind::CaptureRequest, &id, 1);
    request.capture_kind = Some(kind.to_string());
    request.target = Some(target.to_string());
    with(|sessions| {
        sessions.insert(
            device_id.to_string(),
            Capture {
                id,
                kind: kind.to_string(),
                target: target.to_string(),
                receiving: false,
                phone: String::new(),
                started: Instant::now(),
                sequence: 1,
            },
        )
    });
    request
}

/// Cancels the open capture here: the message to send, if one was open.
pub(crate) fn cancel(device_id: &str, build: Build) -> Option<ControlMessage> {
    let capture = with(|sessions| sessions.remove(device_id)).flatten()?;
    let mut cancelled = build(
        ControlKind::CaptureCancelled,
        &capture.id,
        capture.sequence + 1,
    );
    cancelled.reason = Some("user".to_string());
    Some(cancelled)
}

/// The peer went: its capture ends with it.
pub(crate) fn end(device_id: &str) {
    with(|sessions| sessions.remove(device_id));
}

/// What a phone's `capture-*` message did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// Text (recognised, or a QR code's contents) for the clipboard.
    Text {
        kind: String,
        text: String,
    },
    /// A file follows.
    AwaitFile,
    /// It ended there; why, in the user's words (None: the user's own doing).
    Ended(Option<String>),
    Ignored,
}

pub(crate) fn handle(device_id: &str, phone: &str, message: &ControlMessage) -> Outcome {
    let id = message.remote_session_id.as_str();
    let current = status(device_id).filter(|capture| capture.id == id);
    let Some(capture) = current else {
        return Outcome::Ignored;
    };
    match message.kind {
        ControlKind::CaptureResult => {
            if let Some(text) = message.text.clone().filter(|text| !text.is_empty()) {
                end(device_id);
                return Outcome::Text {
                    kind: capture.kind,
                    text,
                };
            }
            if message.mime_type.is_some() {
                with(|sessions| {
                    if let Some(open) = sessions.get_mut(device_id) {
                        open.receiving = true;
                        open.phone = phone.to_string();
                    }
                });
                return Outcome::AwaitFile;
            }
            end(device_id);
            Outcome::Ended(Some(format!("Nothing came back from {phone}")))
        }
        ControlKind::CaptureCancelled => {
            end(device_id);
            Outcome::Ended(reason_words(phone, message.reason.as_deref()))
        }
        // The desk is never asked for a capture.
        _ => Outcome::Ignored,
    }
}

/// A capture's file arrived: ends it. One for the clipboard is announced
/// here; a saved one already gets the Transfers toast with Show in Folder.
pub(crate) fn file_arrived(capture_id: &str) {
    let ended = with(|sessions| {
        let device = sessions
            .iter()
            .find(|(_, capture)| capture.id == capture_id)
            .map(|(device, _)| device.clone())?;
        sessions.remove(&device).map(|capture| (device, capture))
    })
    .flatten();
    let Some((device_id, capture)) = ended else {
        return;
    };
    let what = match capture.kind.as_str() {
        "scan" => "Scan",
        _ => "Photo",
    };
    if capture.target != "file" {
        notify(DaemonEvent::CaptureNotice {
            device_id,
            message: format!("{what} from {} copied — paste it anywhere", capture.phone),
        });
    }
}

/// Why the phone ended it, in the user's words; `None` for the user's own
/// cancel there.
pub(crate) fn reason_words(phone: &str, reason: Option<&str>) -> Option<String> {
    Some(match reason {
        Some("user") => return None,
        Some("timeout") => format!("{phone} was not used in time"),
        Some("permission") => format!("DeskLink may not use the camera on {phone}"),
        Some("unsupported") => format!("{phone} cannot do that"),
        _ => format!("{phone} stopped"),
    })
}

pub(crate) fn notify(event: DaemonEvent) {
    if let Ok(mut notices) = NOTICES.lock() {
        // The window takes them once a second; with none open they must not
        // pile up.
        if notices.len() >= MAX_NOTICES {
            notices.remove(0);
        }
        notices.push(event);
    }
    crate::device_links::daemon::state::wake_ui();
}

/// Notices for the window, taken once.
pub(crate) fn take_notices() -> Vec<DaemonEvent> {
    NOTICES
        .lock()
        .map(|mut notices| std::mem::take(&mut *notices))
        .unwrap_or_default()
}

/// A Wi-Fi network from a QR code (`WIFI:S:<ssid>;T:<WPA|WEP|nopass>;P:<password>;H:<true>;;`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wifi {
    pub ssid: String,
    /// "WPA", "WEP", or empty for an open network.
    pub security: String,
    pub password: String,
    pub hidden: bool,
}

/// Parses a QR code's Wi-Fi payload; `None` when it is not one.
pub(crate) fn wifi(text: &str) -> Option<Wifi> {
    let body = text.trim().strip_prefix("WIFI:")?;
    let mut fields = HashMap::new();
    let (mut key, mut value, mut in_value, mut escaped) =
        (String::new(), String::new(), false, false);
    for c in body.chars() {
        if escaped {
            if in_value {
                value.push(c);
            } else {
                key.push(c);
            }
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else if !in_value && c == ':' {
            in_value = true;
        } else if c == ';' {
            if !key.is_empty() {
                fields.insert(std::mem::take(&mut key), std::mem::take(&mut value));
            }
            in_value = false;
        } else if in_value {
            value.push(c);
        } else {
            key.push(c);
        }
    }
    let ssid = fields.remove("S").filter(|ssid| !ssid.is_empty())?;
    // What the code says, honestly: only open, WEP and WPA/WPA2 passphrase
    // networks can be joined from here; WPA3 (SAE), enterprise and anything
    // unknown are named as what they are and refused when joining.
    let security = match fields.remove("T").as_deref().map(str::to_ascii_uppercase) {
        Some(kind) => match kind.as_str() {
            "WEP" => "WEP",
            "NOPASS" | "" => "",
            "WPA" | "WPA2" | "WPA/WPA2" | "WPA2-PSK" | "WPA-PSK" => "WPA",
            "SAE" | "WPA3" | "WPA3-SAE" => "SAE",
            "EAP" | "WPA-EAP" | "WPA2-EAP" | "WPA3-EAP" | "EAP-TLS" | "PEAP" => "EAP",
            _ => "OTHER",
        },
        None => "",
    }
    .to_string();
    Some(Wifi {
        ssid,
        password: fields.remove("P").unwrap_or_default(),
        hidden: fields.remove("H").as_deref() == Some("true"),
        security,
    })
}

/// What NetworkManager would refuse, said in plain words first: a name over
/// 32 bytes, a WPA passphrase that is not 8 to 63 characters (or 64 hex
/// digits), a WEP key of the wrong length.
fn check_wifi(network: &Wifi) -> Result<(), String> {
    if network.ssid.len() > 32 {
        return Err("That network's name is too long to be a Wi-Fi network".to_string());
    }
    let key = network.password.as_str();
    let hex64 = key.len() == 64 && key.bytes().all(|byte| byte.is_ascii_hexdigit());
    // A WPA passphrase is 8 to 63 printable ASCII characters (bytes: a
    // letter with an accent is two), or 64 hex digits.
    let ascii = key.bytes().all(|byte| (0x20..=0x7e).contains(&byte));
    match network.security.as_str() {
        "SAE" => Err("That network uses WPA3, which DeskLink cannot join yet. Join it in the system's Wi-Fi settings".to_string()),
        "EAP" => Err("That is a company or school network (it needs a login). Join it in the system's Wi-Fi settings".to_string()),
        "OTHER" => Err("DeskLink does not know that kind of Wi-Fi security. Join it in the system's Wi-Fi settings".to_string()),
        "WPA" if !(hex64 || ascii && (8..=63).contains(&key.len())) => {
            Err("That Wi-Fi password is not a valid one (8 to 63 plain characters)".to_string())
        }
        "WEP" if !matches!(key.len(), 5 | 10 | 13 | 26) => {
            Err("That Wi-Fi key is not a valid one".to_string())
        }
        _ => Ok(()),
    }
}

/// Joins a Wi-Fi network through NetworkManager (the user asked, from a QR
/// code the phone scanned): a new connection on the first Wi-Fi device.
pub fn join_wifi(network: &Wifi) -> Result<(), String> {
    check_wifi(network)?;
    use gtk::gio;
    use gtk::glib::{self, prelude::ToVariant};
    let bus = gio::bus_get_sync(gio::BusType::System, None::<&gio::Cancellable>)
        .map_err(|error| format!("No system bus: {error}"))?;
    let call = |path: &str, interface: &str, method: &str, args: Option<glib::Variant>| {
        bus.call_sync(
            Some("org.freedesktop.NetworkManager"),
            path,
            interface,
            method,
            args.as_ref(),
            None,
            gio::DBusCallFlags::NONE,
            10_000,
            None::<&gio::Cancellable>,
        )
        .map_err(|error| format!("NetworkManager: {error}"))
    };
    let devices = call(
        "/org/freedesktop/NetworkManager",
        "org.freedesktop.NetworkManager",
        "GetDevices",
        None,
    )?
    .child_value(0);
    let wifi_device = (0..devices.n_children())
        .filter_map(|index| devices.child_value(index).str().map(str::to_string))
        .find(|path| {
            call(
                path,
                "org.freedesktop.DBus.Properties",
                "Get",
                Some(("org.freedesktop.NetworkManager.Device", "DeviceType").to_variant()),
            )
            .ok()
            .and_then(|reply| reply.child_value(0).as_variant())
            .and_then(|value| value.get::<u32>())
                == Some(2)
        })
        .ok_or_else(|| "This computer has no Wi-Fi".to_string())?;
    let section = |entries: Vec<(&str, glib::Variant)>| {
        let dict = glib::VariantDict::new(None);
        for (key, value) in entries {
            dict.insert_value(key, &value);
        }
        dict.end()
    };
    let mut settings = vec![
        (
            "connection",
            section(vec![
                ("id", network.ssid.to_variant()),
                ("type", "802-11-wireless".to_variant()),
            ]),
        ),
        (
            "802-11-wireless",
            section(vec![
                ("ssid", network.ssid.as_bytes().to_variant()),
                ("mode", "infrastructure".to_variant()),
                ("hidden", network.hidden.to_variant()),
            ]),
        ),
    ];
    match network.security.as_str() {
        "WPA" => settings.push((
            "802-11-wireless-security",
            section(vec![
                ("key-mgmt", "wpa-psk".to_variant()),
                ("psk", network.password.to_variant()),
            ]),
        )),
        "WEP" => settings.push((
            "802-11-wireless-security",
            section(vec![
                ("key-mgmt", "none".to_variant()),
                ("wep-key0", network.password.to_variant()),
            ]),
        )),
        _ => {}
    }
    let connection: HashMap<String, glib::Variant> = settings
        .into_iter()
        .map(|(name, value)| (name.to_string(), value))
        .collect();
    let device = glib::variant::ObjectPath::try_from(wifi_device.as_str())
        .map_err(|_| "NetworkManager returned an invalid device".to_string())?;
    let root = glib::variant::ObjectPath::try_from("/").map_err(|error| error.to_string())?;
    call(
        "/org/freedesktop/NetworkManager",
        "org.freedesktop.NetworkManager",
        "AddAndActivateConnection",
        Some((connection, device, root).to_variant()),
    )
    .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device_links::webrtc::WebRtcWireBinding;

    /// The notices are one queue for the whole process: tests that read it
    /// take turns.
    fn notices_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn build(kind: ControlKind, id: &str, sequence: i64) -> ControlMessage {
        let wire = WebRtcWireBinding::from_attempt(
            "desktop",
            "phone",
            "01234567-89ab-cdef-0123-456789abcdef",
        )
        .unwrap();
        ControlMessage::new(kind, "attempt", &wire, id, sequence, 1)
    }

    #[test]
    fn a_request_carries_capture_kind_and_its_text_result_ends_it() {
        let request = begin("cap-a", "text", "weird", &build);
        assert_eq!(request.kind, ControlKind::CaptureRequest);
        assert_eq!(request.capture_kind.as_deref(), Some("text"));
        assert_eq!(
            request.target.as_deref(),
            Some("clipboard"),
            "unknown is clipboard"
        );
        let json: serde_json::Value = serde_json::from_slice(&request.to_json()).unwrap();
        assert_eq!(json["kind"], "capture-request");
        assert_eq!(json["captureKind"], "text");
        let id = request.remote_session_id.clone();

        let mut stranger = build(
            ControlKind::CaptureResult,
            &uuid::Uuid::new_v4().to_string(),
            1,
        );
        stranger.text = Some("nope".into());
        assert_eq!(handle("cap-a", "Pixel", &stranger), Outcome::Ignored);

        let mut result = build(ControlKind::CaptureResult, &id, 1);
        result.text = Some("Hello from a picture".into());
        assert_eq!(
            handle("cap-a", "Pixel", &result),
            Outcome::Text {
                kind: "text".into(),
                text: "Hello from a picture".into()
            }
        );
        assert!(status("cap-a").is_none());
    }

    #[test]
    fn a_photo_waits_for_its_file_and_a_cancel_says_why() {
        let _turn = notices_lock();
        let request = begin("cap-b", "photo", "file", &build);
        let id = request.remote_session_id.clone();
        let mut result = build(ControlKind::CaptureResult, &id, 1);
        result.mime_type = Some("image/jpeg".into());
        assert_eq!(handle("cap-b", "Pixel", &result), Outcome::AwaitFile);
        assert!(status("cap-b").unwrap().receiving);
        file_arrived(&id);
        assert!(status("cap-b").is_none());

        let copied = begin("cap-b", "photo", "clipboard", &build);
        let mut photo = build(ControlKind::CaptureResult, &copied.remote_session_id, 1);
        photo.mime_type = Some("image/jpeg".into());
        assert_eq!(handle("cap-b", "Pixel", &photo), Outcome::AwaitFile);
        file_arrived(&copied.remote_session_id);
        assert!(take_notices().iter().any(|event| matches!(
            event,
            DaemonEvent::CaptureNotice { message, .. } if message == "Photo from Pixel copied — paste it anywhere"
        )));

        let again = begin("cap-b", "scan", "file", &build);
        let mut timeout = build(ControlKind::CaptureCancelled, &again.remote_session_id, 2);
        timeout.reason = Some("timeout".into());
        assert_eq!(
            handle("cap-b", "Pixel", &timeout),
            Outcome::Ended(Some("Pixel was not used in time".into()))
        );
        let third = begin("cap-b", "qr", "clipboard", &build);
        let cancelled = cancel("cap-b", &build).unwrap();
        assert_eq!(cancelled.remote_session_id, third.remote_session_id);
        assert_eq!(cancelled.reason.as_deref(), Some("user"));
        assert!(cancel("cap-b", &build).is_none());
    }

    #[test]
    fn a_capture_nobody_answered_ends_with_a_word() {
        let _turn = notices_lock();
        take_notices();
        with(|sessions| {
            sessions.insert(
                "slow-phone".into(),
                Capture {
                    id: "c".into(),
                    kind: "photo".into(),
                    target: "clipboard".into(),
                    receiving: false,
                    phone: String::new(),
                    started: Instant::now() - FORGET_AFTER - Duration::from_secs(1),
                    sequence: 1,
                },
            );
        });
        assert!(status("slow-phone").is_none());
        let notices = take_notices();
        assert!(notices.iter().any(|event| matches!(
            event,
            DaemonEvent::CaptureNotice { message, .. } if message.contains("did not answer")
        )));
    }

    #[test]
    fn a_wifi_code_nobody_could_join_is_refused_in_plain_words() {
        let network = |security: &str, ssid: &str, password: &str| Wifi {
            ssid: ssid.into(),
            security: security.into(),
            password: password.into(),
            hidden: false,
        };
        assert!(check_wifi(&network("WPA", "home", "correct horse")).is_ok());
        assert!(check_wifi(&network("WPA", "home", &"f".repeat(64))).is_ok());
        assert!(check_wifi(&network("", "cafe", "")).is_ok());
        assert!(check_wifi(&network("WPA", "home", "short")).is_err());
        assert!(check_wifi(&network("WPA", "home", &"x".repeat(64))).is_err());
        assert!(check_wifi(&network("WPA", &"n".repeat(33), "correct horse")).is_err());
        assert!(check_wifi(&network("WEP", "old", "12345")).is_ok());
        assert!(check_wifi(&network("WEP", "old", "123")).is_err());
        // Bytes, not characters: 32 two-byte letters are 64 bytes.
        assert!(check_wifi(&network("WPA", "home", &"é".repeat(32))).is_err());
        assert!(check_wifi(&network("WPA", "home", &"é".repeat(5))).is_err());
        assert!(check_wifi(&network("WPA", "home", "tab\there1")).is_err());
        // WPA3 and company networks are said to be what they are.
        let sae = check_wifi(&network("SAE", "home", "correct horse")).unwrap_err();
        assert!(sae.contains("WPA3"), "{sae}");
        let eap = check_wifi(&network("EAP", "campus", "")).unwrap_err();
        assert!(eap.contains("company or school"), "{eap}");
        assert!(check_wifi(&network("OTHER", "odd", "x")).is_err());
    }

    #[test]
    fn notices_for_a_window_that_is_not_there_do_not_pile_up() {
        let _turn = notices_lock();
        take_notices();
        for index in 0..(MAX_NOTICES * 3) {
            notify(DaemonEvent::CaptureNotice {
                device_id: "p".into(),
                message: index.to_string(),
            });
        }
        let kept = take_notices();
        assert_eq!(kept.len(), MAX_NOTICES);
        assert!(matches!(
            kept.last(),
            Some(DaemonEvent::CaptureNotice { message, .. }) if *message == (MAX_NOTICES * 3 - 1).to_string()
        ));
    }

    #[test]
    fn wifi_codes_are_parsed_with_escapes() {
        assert_eq!(
            wifi(r"WIFI:S:My\;Net;T:WPA;P:pa\:ss\\word;H:true;;"),
            Some(Wifi {
                ssid: "My;Net".into(),
                security: "WPA".into(),
                password: r"pa:ss\word".into(),
                hidden: true,
            })
        );
        assert_eq!(
            wifi("WIFI:T:nopass;S:Cafe;;").map(|w| (w.ssid, w.security)),
            Some(("Cafe".into(), String::new()))
        );
        let kind = |code: &str| wifi(code).map(|w| w.security);
        assert_eq!(kind("WIFI:S:a;T:SAE;P:x;;").as_deref(), Some("SAE"));
        assert_eq!(kind("WIFI:S:a;T:WPA3;P:x;;").as_deref(), Some("SAE"));
        assert_eq!(kind("WIFI:S:a;T:WPA2-EAP;;").as_deref(), Some("EAP"));
        assert_eq!(kind("WIFI:S:a;T:WPA2;P:x;;").as_deref(), Some("WPA"));
        assert_eq!(kind("WIFI:S:a;T:mystery;;").as_deref(), Some("OTHER"));
        assert_eq!(wifi("https://example.com"), None);
        assert_eq!(wifi("WIFI:T:WPA;P:x;;"), None, "no network name");
    }
}
