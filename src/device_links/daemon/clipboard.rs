use std::io::Write;
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
        let links = Arc::clone(&self.links);
        thread::spawn(move || loop {
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

                if let Ok(mut links_locked) = links.lock() {
                    for (device_id, link) in links_locked.iter_mut() {
                        if link.pairing.state == PairState::Paired {
                            let mut packet = NetworkPacket::new(PACKET_TYPE_CLIPBOARD);
                            packet.set("content", current_text.clone());
                            eprintln!(
                                "[Daemon] Local clipboard changed. Syncing to device {}: {:?}",
                                device_id, current_text
                            );
                            if let Ok(mut stream) = link.stream.lock() {
                                if let Ok(bytes) = packet.serialize_line() {
                                    let _ = stream.write_all(&bytes);
                                }
                            }
                        }
                    }
                }
            }
        });
    }
}
