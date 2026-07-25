use std::sync::atomic::Ordering;
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::DaemonWorker;
use crate::device_links::core::events::CoreEvent;
use crate::device_links::packet::{NetworkPacket, PACKET_TYPE_CLIPBOARD};
use crate::device_links::pairing::PairState;

static LAST_CLIPBOARD: std::sync::OnceLock<Mutex<String>> = std::sync::OnceLock::new();
pub(super) const MAX_CLIPBOARD_BYTES: usize = 1024 * 1024;

pub(super) fn get_last_clipboard() -> &'static Mutex<String> {
    LAST_CLIPBOARD.get_or_init(|| Mutex::new(String::new()))
}

pub(super) fn set_clipboard_text_from_remote(content: &str) {
    if content.len() > MAX_CLIPBOARD_BYTES {
        eprintln!("[Daemon] Rejecting oversized clipboard payload");
        return;
    }
    if let Ok(mut last) = get_last_clipboard().lock() {
        *last = content.to_string();
    }

    let content = content.to_string();
    thread::spawn(move || {
        if let Ok(mut clipboard) = arboard::Clipboard::new() {
            let _ = clipboard.set_text(content);
            thread::sleep(Duration::from_secs(5));
        }
    });
}

impl DaemonWorker {
    pub(super) fn start_clipboard_listener(&self) {
        let sessions = self.sessions.clone();
        let events = self.events.clone();
        let shutdown = std::sync::Arc::clone(&self.shutdown);
        thread::spawn(move || loop {
            if shutdown.load(Ordering::SeqCst) {
                break;
            }
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
            if current_text.len() > MAX_CLIPBOARD_BYTES {
                eprintln!("[Daemon] Ignoring oversized local clipboard content");
                continue;
            }

            let Ok(mut last) = get_last_clipboard().lock() else {
                continue;
            };
            if *last != current_text {
                *last = current_text.clone();
                drop(last);

                for session in sessions.sessions_snapshot() {
                    if session.pair_state == PairState::Paired {
                        if let Some(binding) = sessions.current_binding(&session.device_id) {
                            if !sessions.is_current(&binding) {
                                continue;
                            }
                            let mut packet = NetworkPacket::new(PACKET_TYPE_CLIPBOARD);
                            packet.set("content", current_text.clone());
                            packet.set("timestamp", now_millis());
                            eprintln!(
                                "[Daemon] Local clipboard changed. Syncing to device {}: {:?}",
                                session.device_id, current_text
                            );
                            let result = sessions
                                .current_webrtc_binding(&session.device_id)
                                .filter(|web_rtc| sessions.is_current_webrtc(web_rtc))
                                .ok_or_else(|| {
                                    "DeskLink WebRTC feature transport is unavailable".to_string()
                                })
                                .and_then(|web_rtc| {
                                    web_rtc
                                        .transport
                                        .send_packet(&packet, now_millis())
                                        .map_err(|error| error.to_string())
                                });
                            if let Err(error) = result {
                                events.publish(CoreEvent::Error {
                                    scope: "clipboard".to_string(),
                                    device_id: Some(session.device_id.clone()),
                                    message: error,
                                    retryable: true,
                                });
                            }
                        }
                    }
                }
            }
        });
    }
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default()
}
