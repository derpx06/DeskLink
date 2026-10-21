use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use super::state::push_error;
use super::DaemonWorker;
use crate::device_links::features::is_webrtc_feature;
use crate::device_links::packet::{NetworkPacket, PACKET_TYPE_CLIPBOARD};
use crate::device_links::pairing::PairState;
use crate::device_links::transport::{should_surface_send_error, FeatureTransport};

static LAST_CLIPBOARD: std::sync::OnceLock<Mutex<String>> = std::sync::OnceLock::new();
static LAST_SENT_BY_DEVICE: std::sync::OnceLock<Mutex<std::collections::HashMap<String, String>>> =
    std::sync::OnceLock::new();

pub(super) fn get_last_clipboard() -> &'static Mutex<String> {
    LAST_CLIPBOARD.get_or_init(|| Mutex::new(String::new()))
}

/// How many clipboard items the Clipboard page lists. Memory only: the
/// history is never written to disk.
const HISTORY_LEN: usize = 20;

/// The MIME type password managers (KeePassXC, KDE's) add to a copied
/// secret. An item carrying it is neither sent nor listed.
const PASSWORD_HINT: &str = "x-kde-passwordManagerHint";

/// One shared clipboard item, for the Clipboard page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardItem {
    pub text: String,
    /// The phone it came from; `None` when it was copied here.
    pub from: Option<String>,
    /// Unix seconds.
    pub at: u64,
}

static HISTORY: Mutex<std::collections::VecDeque<ClipboardItem>> =
    Mutex::new(std::collections::VecDeque::new());

/// The shared items, newest first.
pub fn history() -> Vec<ClipboardItem> {
    HISTORY
        .lock()
        .map(|history| history.iter().cloned().collect())
        .unwrap_or_default()
}

/// Debug builds only: lists an item, for screenshots without a phone.
#[cfg(debug_assertions)]
pub fn debug_remember(text: &str, from: Option<&str>) {
    remember(text, from);
}

/// Debug builds only: a phone's clipboard arriving, for isolated tests.
#[cfg(debug_assertions)]
pub fn debug_receive(text: &str) {
    let outcome = clipboard_from_phone(text, "Test phone", None);
    debug_log!("[Clipboard] test copy: {outcome}");
}

fn remember(text: &str, from: Option<&str>) {
    let at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    if let Ok(mut history) = HISTORY.lock() {
        push_history(
            &mut history,
            ClipboardItem {
                text: text.to_string(),
                from: from.map(str::to_string),
                at,
            },
        );
    }
}

/// Newest first, capped, and an item copied again moves to the top instead
/// of being listed twice.
fn push_history(history: &mut std::collections::VecDeque<ClipboardItem>, item: ClipboardItem) {
    if item.text.trim().is_empty() {
        return;
    }
    history.retain(|known| known.text != item.text);
    history.push_front(item);
    history.truncate(HISTORY_LEN);
}

fn is_password_hint(targets: &[String]) -> bool {
    targets.iter().any(|target| target == PASSWORD_HINT)
}

/// Whether password-manager hints can be read here: 0 not yet known, 1 yes,
/// 2 no (no X display to ask, so no XWayland either).
static DETECTION: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

/// Whether this session can tell a copied password from other text; `None`
/// until it has been tried. Without it, the UI must not promise that
/// passwords stay on this computer.
pub fn password_detection() -> Option<bool> {
    match DETECTION.load(Ordering::Acquire) {
        1 => Some(true),
        2 => Some(false),
        _ => None,
    }
}

fn record_detection(available: bool) {
    DETECTION.store(if available { 1 } else { 2 }, Ordering::Release);
}

/// Whether the clipboard's owner marked its content as a password. Read
/// from the X11 CLIPBOARD selection's TARGETS, the same selection `arboard`
/// reads (GNOME mirrors Wayland's clipboard into it). An owner that does not
/// answer counts as unmarked; no display at all is recorded as detection
/// being unavailable (see [`password_detection`]). Runs on the clipboard
/// thread: the reply wait is bounded at 300 ms, never on the UI's thread.
fn clipboard_is_password() -> bool {
    clipboard_targets().is_some_and(|targets| is_password_hint(&targets))
}

fn clipboard_targets() -> Option<Vec<String>> {
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{AtomEnum, ConnectionExt, CreateWindowAux, WindowClass};
    use x11rb::protocol::Event;

    let connected = x11rb::connect(None).ok();
    record_detection(connected.is_some());
    let (conn, screen) = connected?;
    let root = conn.setup().roots.get(screen)?.root;
    let intern = |name: &[u8]| Some(conn.intern_atom(false, name).ok()?.reply().ok()?.atom);
    let clipboard = intern(b"CLIPBOARD")?;
    let targets = intern(b"TARGETS")?;
    let property = intern(b"DESKLINK_TARGETS")?;
    let window = conn.generate_id().ok()?;
    conn.create_window(
        0,
        window,
        root,
        0,
        0,
        1,
        1,
        0,
        WindowClass::INPUT_ONLY,
        0,
        &CreateWindowAux::new(),
    )
    .ok()?;
    conn.convert_selection(window, clipboard, targets, property, x11rb::CURRENT_TIME)
        .ok()?;
    conn.flush().ok()?;
    let deadline = std::time::Instant::now() + Duration::from_millis(300);
    while std::time::Instant::now() < deadline {
        match conn.poll_for_event().ok()? {
            Some(Event::SelectionNotify(notify)) if notify.requestor == window => {
                if notify.property == u32::from(AtomEnum::NONE) {
                    return None;
                }
                let reply = conn
                    .get_property(true, window, property, AtomEnum::ATOM, 0, 1024)
                    .ok()?
                    .reply()
                    .ok()?;
                let names = reply
                    .value32()?
                    .filter_map(|atom| conn.get_atom_name(atom).ok()?.reply().ok())
                    .map(|reply| String::from_utf8_lossy(&reply.name).into_owned())
                    .collect();
                return Some(names);
            }
            Some(_) => {}
            None => thread::sleep(Duration::from_millis(10)),
        }
    }
    None
}

/// When a phone's copy last arrived, and when the desk's own clipboard last
/// changed (Unix milliseconds; 0 never).
static REMOTE_AT: AtomicU64 = AtomicU64::new(0);
static LOCAL_CHANGED_AT: AtomicU64 = AtomicU64::new(0);

/// How long a phone's copy may take to land on this clipboard. Text first
/// seen here before that is older than the phone's copy and is never sent
/// back over it.
const SETTLE_MS: u64 = 1500;

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64)
}

/// Whether desk text first seen at `seen_ms` was replaced by a phone's copy
/// received at `remote_ms`: it is then not a new copy to share (the phone's
/// copy is the newer one, even while it is still landing or if it could not
/// be placed).
fn superseded(seen_ms: u64, remote_ms: u64) -> bool {
    remote_ms != 0 && seen_ms < remote_ms.saturating_add(SETTLE_MS)
}

/// `clipboard.connect` (sent on every connection) is the phone's clipboard
/// as of `timestamp`; it replaces the desk's only when newer than the desk's
/// own last change. An unknown time (absent or 0) never does.
fn connect_is_newer(timestamp: Option<u64>, local_changed_ms: u64) -> bool {
    timestamp.is_some_and(|at| at > 0 && at > local_changed_ms)
}

/// What a phone's clipboard packet did here, for the debug log.
pub(super) fn clipboard_from_phone(
    content: &str,
    device_name: &str,
    connect_timestamp: Option<Option<u64>>,
) -> &'static str {
    if !crate::device_links::config::app_settings().clipboard_sync {
        return "ignored: clipboard sync is off";
    }
    if get_last_clipboard()
        .lock()
        .is_ok_and(|last| *last == content)
    {
        return "unchanged";
    }
    if let Some(timestamp) = connect_timestamp {
        if !connect_is_newer(timestamp, LOCAL_CHANGED_AT.load(Ordering::Acquire)) {
            return "ignored: older than this computer's clipboard";
        }
    }
    remember(content, Some(device_name));
    set_clipboard_text_from_remote(content);
    "placed on the clipboard"
}

/// The X11 clipboard context, kept for the life of the process: `arboard`
/// serves our selection from it, so it must outlive every paste. On GNOME
/// and KDE Wayland the compositor mirrors this X11 selection (through
/// XWayland) to every Wayland application.
static X11_CLIPBOARD: Mutex<Option<arboard::Clipboard>> = Mutex::new(None);

fn set_x11(text: &str) -> bool {
    let Ok(mut held) = X11_CLIPBOARD.lock() else {
        return false;
    };
    // A second try with a fresh context: the first may be dead (XWayland
    // restarted), and a dead one would refuse every later copy.
    for _ in 0..2 {
        if held.is_none() {
            *held = arboard::Clipboard::new().ok();
        }
        let Some(clipboard) = held.as_mut() else {
            return false;
        };
        if clipboard.set_text(text.to_string()).is_ok() {
            return true;
        }
        *held = None;
    }
    false
}

/// Publishes clipboard text received from a paired device.
///
/// Through X11 first. A Wayland client cannot do this: Mutter (and KWin)
/// accept a Wayland clipboard only from the focused window, and the phone's
/// copy arrives while DeskLink is hidden or in the background — GTK's
/// `Gdk.Clipboard` then claims it locally and nobody else ever sees it.
/// Verified on GNOME Shell 50: X11 text from an unfocused client pastes in
/// Wayland applications; GTK text from an unfocused (or windowless) client
/// does not. With no X display at all, GTK is the last resort: it works
/// while a DeskLink window has focus.
pub(super) fn set_clipboard_text_from_remote(content: &str) {
    REMOTE_AT.store(now_ms(), Ordering::Release);
    LOCAL_CHANGED_AT.store(now_ms(), Ordering::Release);
    if let Ok(mut last) = get_last_clipboard().lock() {
        *last = content.to_string();
    }
    let content = content.to_string();
    // Off the packet thread: the first X11 connection can start XWayland.
    thread::spawn(move || {
        // A newer copy may have arrived meanwhile: only the latest is set.
        let latest = || {
            get_last_clipboard()
                .lock()
                .is_ok_and(|last| *last == content)
        };
        if !latest() || set_x11(&content) {
            return;
        }
        debug_log!("[Clipboard] No X11 clipboard here; placing it through GTK instead");
        gtk::glib::MainContext::default().invoke(move || {
            if let Some(display) = gtk::gdk::Display::default() {
                gtk::prelude::DisplayExt::clipboard(&display).set_text(&content);
            }
        });
    });
}

impl DaemonWorker {
    pub(super) fn start_clipboard_listener(&self) {
        if !is_webrtc_feature(PACKET_TYPE_CLIPBOARD) {
            return;
        }
        let sessions = Arc::clone(&self.sessions);
        let shutdown = Arc::clone(&self.shutdown);
        let errors = Arc::clone(&self.errors);
        thread::spawn(move || {
            // The last local text looked at, whether it was a password (so
            // the selection is asked once per copy, not every second), and
            // when it was first seen.
            let mut examined: Option<(String, bool, u64)> = None;
            // Learn up front whether passwords can be recognised here, so the
            // Clipboard page can say so before anything is copied.
            record_detection(x11rb::connect(None).is_ok());
            while !shutdown.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(1000));
                if !crate::device_links::config::app_settings().clipboard_sync {
                    continue;
                }

                let mut clipboard = match arboard::Clipboard::new() {
                    Ok(c) => c,
                    Err(_) => continue,
                };

                let current_text = match clipboard.get_text() {
                    Ok(t) => t,
                    Err(_) => continue,
                };

                if current_text.is_empty() {
                    continue;
                }

                let Ok(last) = get_last_clipboard().lock() else {
                    continue;
                };
                if *last == current_text {
                    continue;
                }
                drop(last);
                if examined
                    .as_ref()
                    .is_none_or(|(text, ..)| *text != current_text)
                {
                    let secret = clipboard_is_password();
                    let seen = now_ms();
                    if !secret {
                        remember(&current_text, None);
                    }
                    if !superseded(seen, REMOTE_AT.load(Ordering::Acquire)) {
                        LOCAL_CHANGED_AT.store(seen, Ordering::Release);
                    }
                    examined = Some((current_text.clone(), secret, seen));
                }
                // A password, or the text a phone's newer copy replaced (still
                // landing, or it could not be placed): never sent.
                if examined.as_ref().is_some_and(|(_, secret, seen)| {
                    *secret || superseded(*seen, REMOTE_AT.load(Ordering::Acquire))
                }) {
                    continue;
                }

                for binding in sessions.current_bindings() {
                    let already_sent = LAST_SENT_BY_DEVICE
                        .get_or_init(|| Mutex::new(std::collections::HashMap::new()))
                        .lock()
                        .ok()
                        .and_then(|sent| sent.get(&binding.device_id).cloned())
                        .is_some_and(|sent| sent == current_text);
                    if already_sent {
                        continue;
                    }
                    let paired = sessions
                        .with_session(&binding, |session| {
                            session.pairing.state == PairState::Paired
                        })
                        .unwrap_or(false);
                    if !paired || !sessions.is_current(&binding) {
                        continue;
                    }
                    // Skip a device with no usable transport instead of
                    // retrying it every second. The send would fail on a closed
                    // socket, and because the attempt is only recorded on
                    // success the same text would be retried forever.
                    let has_transport = !binding.link.is_closed()
                        || sessions
                            .feature_transport_snapshot(&binding)
                            .map(|snapshot| snapshot.web_rtc_ready)
                            .unwrap_or(false);
                    if !has_transport {
                        continue;
                    }
                    // Sent only over a ready link, never queued: the copy is
                    // latest-wins, and one queued then dropped was marked sent
                    // and never retried.
                    let result =
                        FeatureTransport::for_binding(&sessions, &binding).and_then(|transport| {
                            if !transport.is_ready() {
                                return Ok(false);
                            }
                            let mut packet = NetworkPacket::new(PACKET_TYPE_CLIPBOARD);
                            packet.set("content", current_text.clone());
                            transport.send(&packet).map(|()| true)
                        });
                    if let Ok(mut sent) = LAST_SENT_BY_DEVICE
                        .get_or_init(|| Mutex::new(std::collections::HashMap::new()))
                        .lock()
                    {
                        remember_if_sent(&mut sent, &binding.device_id, &current_text, &result);
                    }
                    match result {
                            Ok(_) => {}
                            Err(error) if should_surface_send_error(&error) => {
                                push_error(
                                    &errors,
                                    format!(
                                        "Could not synchronize clipboard over WebRTC: {error}"
                                    ),
                                );
                            }
                            Err(error) => eprintln!(
                                "[DeskLink] Clipboard synchronization waiting for WebRTC recovery: {error}"
                            ),
                        }
                }
            }
        });
    }
}

/// Records `text` as sent to `device_id` only when it really went.
fn remember_if_sent(
    sent: &mut std::collections::HashMap<String, String>,
    device_id: &str,
    text: &str,
    result: &Result<bool, String>,
) {
    if matches!(result, Ok(true)) {
        sent.insert(device_id.to_string(), text.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_copy_counts_as_sent_only_after_a_real_send() {
        let mut sent = std::collections::HashMap::new();
        remember_if_sent(&mut sent, "phone", "a", &Ok(false));
        assert!(sent.is_empty(), "not ready: tried again later");
        remember_if_sent(
            &mut sent,
            "phone",
            "a",
            &Err("Session link is closed".into()),
        );
        assert!(sent.is_empty(), "failed: tried again later");
        remember_if_sent(&mut sent, "phone", "a", &Ok(true));
        assert_eq!(sent.get("phone").map(String::as_str), Some("a"));
    }

    fn item(text: &str) -> ClipboardItem {
        ClipboardItem {
            text: text.into(),
            from: None,
            at: 0,
        }
    }

    #[test]
    fn the_history_keeps_the_newest_twenty_once_each() {
        let mut history = std::collections::VecDeque::new();
        for n in 0..25 {
            push_history(&mut history, item(&n.to_string()));
        }
        assert_eq!(history.len(), HISTORY_LEN);
        assert_eq!(history.front().unwrap().text, "24");
        assert_eq!(history.back().unwrap().text, "5");

        // Copied again: moved to the top, not listed twice.
        push_history(&mut history, item("10"));
        assert_eq!(history.len(), HISTORY_LEN);
        assert_eq!(history.front().unwrap().text, "10");
        assert_eq!(history.iter().filter(|known| known.text == "10").count(), 1);

        push_history(&mut history, item("  "));
        assert_eq!(
            history.front().unwrap().text,
            "10",
            "blank text is not listed"
        );
    }

    #[test]
    fn a_phone_copy_is_never_sent_back_over_by_older_desk_text() {
        // Desk text seen before the phone's copy arrived, or while it was
        // landing, is older: not sent (it would overwrite the phone's copy).
        assert!(superseded(1_000, 2_000));
        assert!(superseded(2_500, 2_000), "still landing");
        // Copied here after the phone's copy landed: a new copy to share.
        assert!(!superseded(2_000 + SETTLE_MS, 2_000));
        assert!(!superseded(5, 0), "no phone copy yet");
    }

    #[test]
    fn a_reconnect_replaces_the_desk_clipboard_only_with_a_newer_copy() {
        assert!(connect_is_newer(Some(2_000), 1_000));
        assert!(
            !connect_is_newer(Some(1_000), 2_000),
            "the desk copied since"
        );
        assert!(!connect_is_newer(Some(0), 0), "unknown time");
        assert!(!connect_is_newer(None, 0));
        assert!(connect_is_newer(Some(1), 0), "nothing copied here yet");
    }

    #[test]
    fn password_manager_items_are_recognised_by_their_hint() {
        let plain = vec!["TARGETS".to_string(), "UTF8_STRING".to_string()];
        assert!(!is_password_hint(&plain));
        let mut secret = plain.clone();
        secret.push(PASSWORD_HINT.to_string());
        assert!(is_password_hint(&secret));
    }
}
