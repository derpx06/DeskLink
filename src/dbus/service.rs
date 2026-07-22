use std::collections::HashMap;
use std::thread;

use futures::StreamExt;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::OwnedValue;

use crate::device_links::core::events::CoreEvent;
use crate::device_links::core::transfer_manager::TransferCheckpointStore;
use crate::device_links::daemon::{DaemonCommand, DaemonHandle};

pub(super) const BUS_NAME: &str = "derx06.desklink.com";
pub(super) const OBJECT_PATH: &str = "/com/desklink/Service";
pub(super) const INTERFACE: &str = "com.desklink.Service";

type Details = HashMap<String, String>;
type DeviceDetails = Vec<HashMap<String, OwnedValue>>;

fn owned_string(value: impl Into<String>) -> OwnedValue {
    OwnedValue::from(zbus::zvariant::Str::from(value.into()))
}

#[derive(Clone)]
struct DeskLinkService {
    daemon: DaemonHandle,
}

#[zbus::interface(name = "com.desklink.Service")]
impl DeskLinkService {
    fn list_devices(&self) -> DeviceDetails {
        self.daemon
            .devices()
            .into_iter()
            .map(|device| {
                let mut details = HashMap::new();
                details.insert("id".to_string(), owned_string(device.id));
                details.insert("name".to_string(), owned_string(device.name));
                details.insert("deviceType".to_string(), owned_string(device.device_type));
                details.insert("address".to_string(), owned_string(device.address));
                details.insert(
                    "protocolVersion".to_string(),
                    device.protocol_version.into(),
                );
                details.insert("status".to_string(), owned_string(device.status.label()));
                details.insert("trusted".to_string(), device.trusted.into());
                details.insert(
                    "lastError".to_string(),
                    owned_string(device.last_error.unwrap_or_default()),
                );
                details
            })
            .collect()
    }

    fn pair(&self, device_id: &str) -> bool {
        self.daemon
            .try_send(DaemonCommand::RequestPair(device_id.to_string()))
    }

    fn unpair(&self, device_id: &str) -> bool {
        self.daemon
            .try_send(DaemonCommand::Unpair(device_id.to_string()))
    }

    fn ping(&self, device_id: &str) -> bool {
        self.daemon
            .try_send(DaemonCommand::SendPing(device_id.to_string()))
    }

    fn share_files(&self, device_id: &str, files: Vec<String>) -> String {
        let mut accepted = Vec::new();
        for file in files {
            let path = std::path::PathBuf::from(&file);
            if path.is_file()
                && self
                    .daemon
                    .try_send(DaemonCommand::SendFile(device_id.to_string(), path))
            {
                accepted.push(file);
            }
        }
        if accepted.is_empty() {
            self.daemon.report_error(
                "share",
                Some(device_id.to_string()),
                "No regular files were accepted for sharing",
                false,
            );
        }
        accepted.join("\n")
    }

    fn share_url(&self, device_id: &str, url: &str) -> bool {
        self.daemon.try_send(DaemonCommand::SendShareText(
            device_id.to_string(),
            url.to_string(),
        ))
    }

    fn set_clipboard(&self, device_id: &str, text: &str) -> bool {
        self.daemon.try_send(DaemonCommand::SendClipboard(
            device_id.to_string(),
            text.to_string(),
        ))
    }

    fn start_transfer(&self, device_id: &str, file: &str) -> String {
        let path = std::path::PathBuf::from(file);
        if !path.is_file() {
            self.daemon.report_error(
                "transfer",
                Some(device_id.to_string()),
                format!("Transfer source is not a regular file: {file}"),
                false,
            );
            return String::new();
        }
        let transfer_id = uuid::Uuid::new_v4().to_string();
        if self.daemon.try_send(DaemonCommand::StartTransfer(
            device_id.to_string(),
            path,
            transfer_id.clone(),
        )) {
            transfer_id
        } else {
            String::new()
        }
    }

    fn cancel_transfer(&self, transfer_id: &str) -> bool {
        !transfer_id.trim().is_empty()
            && self
                .daemon
                .try_send(DaemonCommand::CancelTransfer(transfer_id.to_string()))
    }

    fn get_transfer(&self, transfer_id: &str) -> HashMap<String, OwnedValue> {
        let mut details = HashMap::new();
        details.insert("transferId".to_string(), owned_string(transfer_id));
        let state_dir = dirs::config_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join("desklink")
            .join("protocol")
            .join("transfers");
        let snapshot = TransferCheckpointStore::new(state_dir)
            .ok()
            .and_then(|store| store.load(transfer_id).ok().flatten())
            .map(|checkpoint| {
                let snapshot =
                    crate::device_links::core::transfer_manager::TransferSnapshot::from_checkpoint(
                        &checkpoint,
                        0,
                    );
                (
                    format!("{:?}", snapshot.state).to_lowercase(),
                    snapshot.bytes_done,
                    snapshot.bytes_total,
                    snapshot.can_resume,
                    snapshot.error.unwrap_or_default(),
                )
            });
        let (state, bytes_done, bytes_total, can_resume, error) =
            snapshot.unwrap_or_else(|| ("unknown".to_string(), 0, 0, false, String::new()));
        details.insert("state".to_string(), owned_string(state));
        details.insert("bytesDone".to_string(), (bytes_done as i64).into());
        details.insert("bytesTotal".to_string(), (bytes_total as i64).into());
        details.insert("canResume".to_string(), can_resume.into());
        details.insert("error".to_string(), owned_string(error));
        details
    }

    fn invoke_feature_action(
        &self,
        device_id: &str,
        action: &str,
        arguments: HashMap<String, String>,
    ) -> bool {
        let command = match action {
            "ping" => DaemonCommand::SendPing(device_id.to_string()),
            "request-notifications" => DaemonCommand::RequestNotifications(device_id.to_string()),
            "request-volume" => DaemonCommand::RequestSystemVolume(device_id.to_string()),
            "request-commands" => DaemonCommand::RequestRemoteCommands(device_id.to_string()),
            "dismiss-notification" => {
                let Some(notification_id) = arguments.get("notificationId") else {
                    return false;
                };
                DaemonCommand::DismissNotification(device_id.to_string(), notification_id.clone())
            }
            "reply-notification" => {
                let (Some(reply_id), Some(message)) =
                    (arguments.get("replyId"), arguments.get("message"))
                else {
                    return false;
                };
                DaemonCommand::ReplyNotification(
                    device_id.to_string(),
                    reply_id.clone(),
                    message.clone(),
                )
            }
            "sftp-open" => DaemonCommand::SendSftpRequest(device_id.to_string()),
            "contacts-sync" => DaemonCommand::RequestContacts(device_id.to_string()),
            "sms-conversations" => DaemonCommand::RequestSmsConversations(device_id.to_string()),
            "telephony-mute" => DaemonCommand::SendTelephonyMute(device_id.to_string()),
            _ => return false,
        };
        self.daemon.try_send(command)
    }

    fn browse_sftp(&self, device_id: &str) -> bool {
        self.daemon
            .try_send(DaemonCommand::SendSftpRequest(device_id.to_string()))
    }

    fn get_preferences(&self) -> HashMap<String, String> {
        [
            ("configPath", "~/.config/desklink/protocol"),
            ("protocolVersion", "9"),
            ("network.discovery", "enabled"),
            ("clipboard.enabled", "enabled"),
            ("downloads.directory", "Downloads"),
            ("security.require-pairing", "true"),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
    }

    fn set_preference(&self, key: &str, value: &str) -> bool {
        self.daemon.try_send(DaemonCommand::SetPreference(
            key.to_string(),
            value.to_string(),
        ))
    }

    #[zbus(signal)]
    async fn device_changed(
        emitter: &SignalEmitter<'_>,
        device_id: &str,
        details: Details,
    ) -> zbus::Result<()> {
        emitter.signal("DeviceChanged", &(device_id, details)).await
    }

    #[zbus(signal)]
    async fn connection_changed(
        emitter: &SignalEmitter<'_>,
        device_id: &str,
        details: Details,
    ) -> zbus::Result<()> {
        emitter
            .signal("ConnectionChanged", &(device_id, details))
            .await
    }

    #[zbus(signal)]
    async fn pairing_changed(
        emitter: &SignalEmitter<'_>,
        device_id: &str,
        details: Details,
    ) -> zbus::Result<()> {
        emitter
            .signal("PairingChanged", &(device_id, details))
            .await
    }

    #[zbus(signal)]
    async fn transfer_changed(
        emitter: &SignalEmitter<'_>,
        transfer_id: &str,
        details: Details,
    ) -> zbus::Result<()> {
        emitter
            .signal("TransferChanged", &(transfer_id, details))
            .await
    }

    #[zbus(signal)]
    async fn feature_state_changed(
        emitter: &SignalEmitter<'_>,
        device_id: &str,
        feature: &str,
        details: Details,
    ) -> zbus::Result<()> {
        emitter
            .signal("FeatureStateChanged", &(device_id, feature, details))
            .await
    }

    #[zbus(signal)]
    async fn notification_received(
        emitter: &SignalEmitter<'_>,
        device_id: &str,
        details: Details,
    ) -> zbus::Result<()> {
        emitter
            .signal("NotificationReceived", &(device_id, details))
            .await
    }

    #[zbus(signal)]
    async fn error(emitter: &SignalEmitter<'_>, scope: &str, details: Details) -> zbus::Result<()> {
        emitter.signal("Error", &(scope, details)).await
    }
}

fn details(entries: impl IntoIterator<Item = (&'static str, String)>) -> Details {
    entries
        .into_iter()
        .map(|(key, value)| (key.to_string(), value))
        .collect()
}

fn emit_event(
    interface: &zbus::blocking::object_server::InterfaceRef<DeskLinkService>,
    event: CoreEvent,
) {
    let emitter = interface.signal_emitter();
    let result = match event {
        CoreEvent::DeviceChanged { device } => {
            futures::executor::block_on(DeskLinkService::device_changed(
                emitter,
                &device.id,
                details([
                    ("name", device.name),
                    ("status", device.status.label().to_string()),
                    ("address", device.address),
                    ("trusted", device.trusted.to_string()),
                    ("protocolVersion", device.protocol_version.to_string()),
                ]),
            ))
        }
        CoreEvent::ConnectionChanged {
            device_id,
            state,
            message,
        } => futures::executor::block_on(DeskLinkService::connection_changed(
            emitter,
            &device_id,
            details([
                ("state", format!("{state:?}")),
                ("message", message.unwrap_or_default()),
            ]),
        )),
        CoreEvent::PairingChanged { device_id, state } => {
            futures::executor::block_on(DeskLinkService::pairing_changed(
                emitter,
                &device_id,
                details([("state", format!("{state:?}"))]),
            ))
        }
        CoreEvent::TransferChanged {
            transfer_id,
            state,
            bytes_done,
            bytes_total,
            can_resume,
            error,
        } => futures::executor::block_on(DeskLinkService::transfer_changed(
            emitter,
            &transfer_id,
            details([
                ("state", state),
                ("bytesDone", bytes_done.to_string()),
                ("bytesTotal", bytes_total.to_string()),
                ("canResume", can_resume.to_string()),
                ("error", error.unwrap_or_default()),
            ]),
        )),
        CoreEvent::FeatureStateChanged {
            device_id,
            feature,
            state,
            details: feature_details,
        } => futures::executor::block_on(DeskLinkService::feature_state_changed(
            emitter,
            &device_id,
            &feature,
            details([("state", state), ("details", feature_details.to_string())]),
        )),
        CoreEvent::NotificationReceived {
            device_id,
            notification,
        } => futures::executor::block_on(DeskLinkService::notification_received(
            emitter,
            &device_id,
            details([
                ("id", notification.id),
                ("title", notification.title),
                ("text", notification.text),
            ]),
        )),
        CoreEvent::Error {
            scope,
            device_id,
            message,
            retryable,
        } => futures::executor::block_on(DeskLinkService::error(
            emitter,
            &scope,
            details([
                ("deviceId", device_id.unwrap_or_default()),
                ("message", message),
                ("retryable", retryable.to_string()),
            ]),
        )),
    };
    if let Err(error) = result {
        eprintln!("[DeskLink] Could not emit D-Bus event: {error}");
    }
}

pub fn start(daemon: DaemonHandle) {
    thread::spawn(move || {
        let events = daemon.subscribe_events();
        let service = DeskLinkService {
            daemon: daemon.clone(),
        };
        let connection = zbus::blocking::connection::Builder::session()
            .and_then(|builder| builder.name(BUS_NAME))
            .and_then(|builder| builder.serve_at(OBJECT_PATH, service))
            .and_then(|builder| builder.build());
        match connection {
            Ok(connection) => {
                let interface = match connection
                    .object_server()
                    .interface::<_, DeskLinkService>(OBJECT_PATH)
                {
                    Ok(interface) => interface,
                    Err(error) => {
                        eprintln!("[DeskLink] D-Bus interface unavailable: {error}");
                        return;
                    }
                };
                let mut events = events;
                while let Some(event) = futures::executor::block_on(events.next()) {
                    emit_event(&interface, event);
                }
            }
            Err(error) => eprintln!("[DeskLink] D-Bus service unavailable: {error}"),
        }
    });
}

#[cfg(test)]
mod tests {
    use super::{BUS_NAME, INTERFACE, OBJECT_PATH};

    #[test]
    fn service_identity_is_stable() {
        assert_eq!(BUS_NAME, "derx06.desklink.com");
        assert_eq!(OBJECT_PATH, "/com/desklink/Service");
        assert_eq!(INTERFACE, "com.desklink.Service");
    }
}
