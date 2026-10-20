use std::sync::atomic::Ordering;
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

/// A phone's clipboard arrived: listed, then published here.
pub(super) fn clipboard_from_phone(content: &str, device_name: &str) {
    remember(content, Some(device_name));
    set_clipboard_text_from_remote(content);
}

/// Publishes clipboard text received from a paired device.
///
/// On X11 the selection has no backing store: the owning process must stay
/// alive to answer paste requests. The previous implementation dropped its
/// `Clipboard` after a five second sleep, so anything pasted later came back
/// empty. `arboard`'s `wait()` keeps ownership until another application takes
/// the selection, which is exactly the required lifetime. Wayland has a real
/// clipboard manager and needs no such handling, and `set_text` is enough
/// there; both paths are covered by keeping the owner thread alive.
pub(super) fn set_clipboard_text_from_remote(content: &str) {
    if let Ok(mut last) = get_last_clipboard().lock() {
        *last = content.to_string();
    }

    let content = content.to_string();
    thread::spawn(move || {
        let Ok(mut clipboard) = arboard::Clipboard::new() else {
            return;
        };
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            use arboard::SetExtLinux;
            // Blocks until ownership is lost, keeping the text pasteable.
            if clipboard.set().wait().text(content.clone()).is_ok() {
                return;
            }
        }
        let _ = clipboard.set_text(content);
        thread::sleep(Duration::from_secs(5));
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
            // The last local text looked at, and whether it was a password,
            // so the selection is asked once per copy, not every second.
            let mut examined: Option<(String, bool)> = None;
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
                    .is_none_or(|(text, _)| *text != current_text)
                {
                    let secret = clipboard_is_password();
                    if !secret {
                        remember(&current_text, None);
                    }
                    examined = Some((current_text.clone(), secret));
                }
                if examined.as_ref().is_some_and(|(_, secret)| *secret) {
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
                    let result =
                        FeatureTransport::for_binding(&sessions, &binding).and_then(|transport| {
                            let mut packet = NetworkPacket::new(PACKET_TYPE_CLIPBOARD);
                            packet.set("content", current_text.clone());
                            transport.send(&packet)
                        });
                    match result {
                            Ok(()) => {
                                if let Ok(mut sent) = LAST_SENT_BY_DEVICE
                                    .get_or_init(|| {
                                        Mutex::new(std::collections::HashMap::new())
                                    })
                                    .lock()
                                {
                                    sent.insert(binding.device_id.clone(), current_text.clone());
                                }
                            }
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn password_manager_items_are_recognised_by_their_hint() {
        let plain = vec!["TARGETS".to_string(), "UTF8_STRING".to_string()];
        assert!(!is_password_hint(&plain));
        let mut secret = plain.clone();
        secret.push(PASSWORD_HINT.to_string());
        assert!(is_password_hint(&secret));
    }
}
