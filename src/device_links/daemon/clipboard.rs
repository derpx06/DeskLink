use std::io::Write;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use super::DaemonWorker;
use crate::device_links::packet::{NetworkPacket, PACKET_TYPE_CLIPBOARD};
use crate::device_links::pairing::PairState;

static LAST_CLIPBOARD: std::sync::OnceLock<Mutex<String>> = std::sync::OnceLock::new();

pub(super) fn get_last_clipboard() -> &'static Mutex<String> {
    LAST_CLIPBOARD.get_or_init(|| Mutex::new(String::new()))
}

pub(super) fn set_clipboard_text_from_remote(content: &str) {
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
        let sessions = Arc::clone(&self.sessions);
        let shutdown = Arc::clone(&self.shutdown);
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

                let Ok(mut last) = get_last_clipboard().lock() else {
                    continue;
                };
                if *last != current_text {
                    *last = current_text.clone();
                    drop(last);

                    for binding in sessions.current_bindings() {
                        let paired = sessions
                            .with_session(&binding, |session| {
                                session.pairing.state == PairState::Paired
                            })
                            .unwrap_or(false);
                        if !paired || !sessions.is_current(&binding) {
                            continue;
                        }
                        let mut packet = NetworkPacket::new(PACKET_TYPE_CLIPBOARD);
                        packet.set("content", current_text.clone());
                        if let (Ok(stream), Ok(bytes)) =
                            (binding.link.stream(), packet.serialize_line())
                        {
                            if let Ok(mut stream) = stream.lock() {
                                let _ = stream.write_all(&bytes);
                            }
                        }
                    }
                }
            }
        });
    }
}
