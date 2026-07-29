//! Bounded decoded VP8 frames for the GTK remote-view surface.
//!
//! The GStreamer pipeline owns decode work. GTK only consumes an already
//! copied RGBA frame on its main thread, which avoids moving GObject/GTK types
//! across the WebRTC worker thread boundary.

use std::collections::HashMap;
use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::{Arc, Mutex, OnceLock};

#[derive(Debug)]
pub struct RemoteVideoFrame {
    pub width: i32,
    pub height: i32,
    pub stride: usize,
    pub rgba: Vec<u8>,
}

type FrameReceiver = Arc<Mutex<Receiver<RemoteVideoFrame>>>;

struct Entry {
    token: String,
    receiver: FrameReceiver,
    input_size: Option<(i32, i32)>,
}

pub fn install(device_id: String, token: String, receiver: Receiver<RemoteVideoFrame>) {
    if let Ok(mut frames) = registry().lock() {
        let input_size = frames.get(&device_id).and_then(|entry| entry.input_size);
        frames.insert(
            device_id,
            Entry {
                token,
                receiver: Arc::new(Mutex::new(receiver)),
                input_size,
            },
        );
    }
}

/// Native target coordinate size advertised by the current remote capture.
/// The rendered VP8 frame may be downscaled, so using the capture size keeps
/// a direct click aligned with the actual Android Accessibility coordinate.
pub fn set_input_size(device_id: &str, width: i32, height: i32) {
    if width <= 0 || height <= 0 {
        return;
    }
    if let Ok(mut frames) = registry().lock() {
        if let Some(entry) = frames.get_mut(device_id) {
            entry.input_size = Some((width, height));
        }
    }
}

pub fn input_size(device_id: &str) -> Option<(i32, i32)> {
    registry()
        .lock()
        .ok()
        .and_then(|frames| frames.get(device_id).and_then(|entry| entry.input_size))
}

pub fn receiver(device_id: &str) -> Option<FrameReceiver> {
    registry()
        .lock()
        .ok()
        .and_then(|frames| frames.get(device_id).map(|entry| Arc::clone(&entry.receiver)))
}

pub fn clear_if_current(device_id: &str, token: &str) {
    if let Ok(mut frames) = registry().lock() {
        if frames
            .get(device_id)
            .is_some_and(|entry| entry.token == token)
        {
            frames.remove(device_id);
        }
    }
}

/// Clears the currently displayed remote frame for an explicit remote-session
/// stop. The caller has already validated the active session/generation, so a
/// paused or revoked stream never leaves a misleading stale screenshot in GTK.
pub fn clear_device(device_id: &str) {
    if let Ok(mut frames) = registry().lock() {
        frames.remove(device_id);
    }
}

pub fn publish(sender: &SyncSender<RemoteVideoFrame>, frame: RemoteVideoFrame) {
    // The viewer needs the newest image, not a queue of stale desktop frames.
    // A full two-frame queue means the rendering thread is behind, so drop the
    // incoming frame and preserve control/data-channel responsiveness.
    let _ = sender.try_send(frame);
}

fn registry() -> &'static Mutex<HashMap<String, Entry>> {
    static REGISTRY: OnceLock<Mutex<HashMap<String, Entry>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}
