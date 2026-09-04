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
            while !shutdown.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(1000));

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
