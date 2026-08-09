use super::state::push_error;
use super::DaemonWorker;
use crate::device_links::transport::FeatureTransport;

impl DaemonWorker {
    pub(super) fn send_file(&self, device_id: &str, file_path: std::path::PathBuf) {
        let transport = match FeatureTransport::for_device(&self.sessions, device_id) {
            Ok(transport) => transport,
            Err(error) => {
                let message =
                    format!("DeskLink file transfer for {device_id} was not started: {error}");
                eprintln!("[Daemon] {message}");
                push_error(&self.errors, message);
                return;
            }
        };
        if !transport.is_ready() {
            let message = format!(
                "DeskLink file transfer for {device_id} requires an authenticated WebRTC file channel"
            );
            eprintln!("[Daemon] {message}");
            push_error(&self.errors, message);
            return;
        }
        if let Err(error) = transport.send_file(&file_path) {
            let message = format!("DeskLink WebRTC file transfer for {device_id} failed: {error}");
            eprintln!("[Daemon] {message}");
            push_error(&self.errors, message);
        }
    }
}
