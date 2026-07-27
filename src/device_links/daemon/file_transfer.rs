use super::state::push_error;
use super::DaemonWorker;
use crate::device_links::webrtc::coordinator::start_file_transfer;

impl DaemonWorker {
    pub(super) fn send_file(&self, device_id: &str, file_path: std::path::PathBuf) {
        let transport = match self
            .sessions
            .current_binding(device_id)
            .ok_or_else(|| "Device is not connected".to_string())
            .and_then(|binding| {
                self.sessions
                    .feature_transport_snapshot(&binding)
                    .map_err(|error| error.to_string())
            }) {
            Ok(transport) => transport,
            Err(error) => {
                let message =
                    format!("DeskLink file transfer for {device_id} was not started: {error}");
                eprintln!("[Daemon] {message}");
                push_error(&self.errors, message);
                return;
            }
        };
        if !transport.web_rtc_ready {
            let message = format!(
                "DeskLink file transfer for {device_id} requires an authenticated WebRTC file channel"
            );
            eprintln!("[Daemon] {message}");
            push_error(&self.errors, message);
            return;
        }
        if let Err(error) = start_file_transfer(&self.sessions, &transport, &file_path) {
            let message = format!("DeskLink WebRTC file transfer for {device_id} failed: {error}");
            eprintln!("[Daemon] {message}");
            push_error(&self.errors, message);
        }
    }
}
