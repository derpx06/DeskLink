use std::collections::HashMap;
use std::thread;

use futures::StreamExt;
use gtk::gio;
use gtk::gio::prelude::*;
use gtk::glib::Variant;
use gtk::prelude::ToVariant;

use crate::device_links::core::events::CoreEvent;
use crate::device_links::core::transfer_manager::TransferCheckpointStore;
use crate::device_links::daemon::{DaemonCommand, DaemonHandle};

use super::service::{BUS_NAME, INTERFACE, OBJECT_PATH};

const INTROSPECTION: &str = r#"<node>
  <interface name="com.desklink.Service">
    <method name="ListDevices"><arg direction="out" type="aa{sv}"/></method>
    <method name="Pair"><arg direction="in" type="s"/><arg direction="out" type="b"/></method>
    <method name="Unpair"><arg direction="in" type="s"/><arg direction="out" type="b"/></method>
    <method name="Ping"><arg direction="in" type="s"/><arg direction="out" type="b"/></method>
    <method name="ShareFiles"><arg direction="in" type="s"/><arg direction="in" type="as"/><arg direction="out" type="s"/></method>
    <method name="ShareUrl"><arg direction="in" type="s"/><arg direction="in" type="s"/><arg direction="out" type="b"/></method>
    <method name="SetClipboard"><arg direction="in" type="s"/><arg direction="in" type="s"/><arg direction="out" type="b"/></method>
    <method name="StartTransfer"><arg direction="in" type="s"/><arg direction="in" type="s"/><arg direction="out" type="s"/></method>
    <method name="CancelTransfer"><arg direction="in" type="s"/><arg direction="out" type="b"/></method>
    <method name="GetTransfer"><arg direction="in" type="s"/><arg direction="out" type="a{sv}"/></method>
    <method name="InvokeFeatureAction"><arg direction="in" type="s"/><arg direction="in" type="s"/><arg direction="in" type="a{ss}"/><arg direction="out" type="b"/></method>
    <method name="BrowseSftp"><arg direction="in" type="s"/><arg direction="out" type="b"/></method>
    <method name="GetPreferences"><arg direction="out" type="a{ss}"/></method>
    <method name="SetPreference"><arg direction="in" type="s"/><arg direction="in" type="s"/><arg direction="out" type="b"/></method>
    <signal name="DeviceChanged"><arg type="s"/><arg type="a{ss}"/></signal>
    <signal name="ConnectionChanged"><arg type="s"/><arg type="a{ss}"/></signal>
    <signal name="PairingChanged"><arg type="s"/><arg type="a{ss}"/></signal>
    <signal name="TransferChanged"><arg type="s"/><arg type="a{ss}"/></signal>
    <signal name="FeatureStateChanged"><arg type="s"/><arg type="s"/><arg type="a{ss}"/></signal>
    <signal name="NotificationReceived"><arg type="s"/><arg type="a{ss}"/></signal>
    <signal name="Error"><arg type="s"/><arg type="a{ss}"/></signal>
  </interface>
</node>"#;

fn string_map<'a>(entries: impl IntoIterator<Item = (&'a str, String)>) -> HashMap<String, String> {
    entries
        .into_iter()
        .map(|(key, value)| (key.to_string(), value))
        .collect()
}

fn device_details(daemon: &DaemonHandle) -> Vec<HashMap<String, Variant>> {
    daemon
        .devices()
        .into_iter()
        .map(|device| {
            [
                ("id", device.id.to_variant()),
                ("name", device.name.to_variant()),
                ("deviceType", device.device_type.to_variant()),
                ("address", device.address.to_variant()),
                ("protocolVersion", device.protocol_version.to_variant()),
                ("status", device.status.label().to_variant()),
                ("trusted", device.trusted.to_variant()),
                (
                    "lastError",
                    device.last_error.unwrap_or_default().to_variant(),
                ),
            ]
            .into_iter()
            .map(|(key, value)| (key.to_string(), value))
            .collect()
        })
        .collect()
}

fn transfer_details(transfer_id: &str) -> HashMap<String, Variant> {
    let state_dir = dirs::config_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("desklink/protocol/transfers");
    let snapshot = TransferCheckpointStore::new(state_dir)
        .ok()
        .and_then(|store| store.load(transfer_id).ok().flatten())
        .map(|checkpoint| {
            crate::device_links::core::transfer_manager::TransferSnapshot::from_checkpoint(
                &checkpoint,
                0,
            )
        });
    let mut result = HashMap::new();
    result.insert("transferId".to_string(), transfer_id.to_variant());
    if let Some(snapshot) = snapshot {
        result.insert(
            "state".to_string(),
            format!("{:?}", snapshot.state).to_lowercase().to_variant(),
        );
        result.insert(
            "bytesDone".to_string(),
            (snapshot.bytes_done as i64).to_variant(),
        );
        result.insert(
            "bytesTotal".to_string(),
            (snapshot.bytes_total as i64).to_variant(),
        );
        result.insert("canResume".to_string(), snapshot.can_resume.to_variant());
        result.insert(
            "error".to_string(),
            snapshot.error.unwrap_or_default().to_variant(),
        );
    } else {
        result.insert("state".to_string(), "unknown".to_variant());
        result.insert("bytesDone".to_string(), 0i64.to_variant());
        result.insert("bytesTotal".to_string(), 0i64.to_variant());
        result.insert("canResume".to_string(), false.to_variant());
        result.insert("error".to_string(), "".to_variant());
    }
    result
}

fn invocation_error(invocation: gio::DBusMethodInvocation, message: &str) {
    invocation.return_dbus_error("com.desklink.Error", message);
}

fn call_method(
    daemon: &DaemonHandle,
    method: &str,
    parameters: &Variant,
) -> Result<Variant, String> {
    match method {
        "ListDevices" => Ok((device_details(daemon),).to_variant()),
        "Pair" => {
            let (id,) = parameters
                .get::<(String,)>()
                .ok_or("Invalid Pair arguments")?;
            Ok((daemon.try_send(DaemonCommand::RequestPair(id)),).to_variant())
        }
        "Unpair" => {
            let (id,) = parameters
                .get::<(String,)>()
                .ok_or("Invalid Unpair arguments")?;
            Ok((daemon.try_send(DaemonCommand::Unpair(id)),).to_variant())
        }
        "Ping" => {
            let (id,) = parameters
                .get::<(String,)>()
                .ok_or("Invalid Ping arguments")?;
            Ok((daemon.try_send(DaemonCommand::SendPing(id)),).to_variant())
        }
        "ShareFiles" => {
            let (id, files) = parameters
                .get::<(String, Vec<String>)>()
                .ok_or("Invalid ShareFiles arguments")?;
            let accepted = files
                .into_iter()
                .filter(|file| {
                    let path = std::path::PathBuf::from(file);
                    path.is_file() && daemon.try_send(DaemonCommand::SendFile(id.clone(), path))
                })
                .collect::<Vec<_>>()
                .join("\n");
            Ok((accepted,).to_variant())
        }
        "ShareUrl" => {
            let (id, url) = parameters
                .get::<(String, String)>()
                .ok_or("Invalid ShareUrl arguments")?;
            Ok((daemon.try_send(DaemonCommand::SendShareText(id, url)),).to_variant())
        }
        "SetClipboard" => {
            let (id, text) = parameters
                .get::<(String, String)>()
                .ok_or("Invalid SetClipboard arguments")?;
            Ok((daemon.try_send(DaemonCommand::SendClipboard(id, text)),).to_variant())
        }
        "StartTransfer" => {
            let (id, file) = parameters
                .get::<(String, String)>()
                .ok_or("Invalid StartTransfer arguments")?;
            let path = std::path::PathBuf::from(file);
            if !path.is_file() {
                return Ok((String::new(),).to_variant());
            }
            let transfer_id = uuid::Uuid::new_v4().to_string();
            let accepted =
                daemon.try_send(DaemonCommand::StartTransfer(id, path, transfer_id.clone()));
            Ok(((if accepted { transfer_id } else { String::new() }),).to_variant())
        }
        "CancelTransfer" => {
            let (id,) = parameters
                .get::<(String,)>()
                .ok_or("Invalid CancelTransfer arguments")?;
            Ok((daemon.try_send(DaemonCommand::CancelTransfer(id)),).to_variant())
        }
        "GetTransfer" => {
            let (id,) = parameters
                .get::<(String,)>()
                .ok_or("Invalid GetTransfer arguments")?;
            Ok((transfer_details(&id),).to_variant())
        }
        "InvokeFeatureAction" => {
            let (id, action, arguments) = parameters
                .get::<(String, String, HashMap<String, String>)>()
                .ok_or("Invalid InvokeFeatureAction arguments")?;
            let command = match action.as_str() {
                "ping" => Some(DaemonCommand::SendPing(id)),
                "request-notifications" => Some(DaemonCommand::RequestNotifications(id)),
                "request-volume" => Some(DaemonCommand::RequestSystemVolume(id)),
                "request-commands" => Some(DaemonCommand::RequestRemoteCommands(id)),
                "sftp-open" => Some(DaemonCommand::SendSftpRequest(id)),
                "dismiss-notification" => arguments
                    .get("notificationId")
                    .map(|value| DaemonCommand::DismissNotification(id, value.clone())),
                "reply-notification" => arguments.get("replyId").zip(arguments.get("message")).map(
                    |(reply, message)| {
                        DaemonCommand::ReplyNotification(id, reply.clone(), message.clone())
                    },
                ),
                _ => None,
            };
            Ok((command
                .map(|command| daemon.try_send(command))
                .unwrap_or(false),)
                .to_variant())
        }
        "BrowseSftp" => {
            let (id,) = parameters
                .get::<(String,)>()
                .ok_or("Invalid BrowseSftp arguments")?;
            Ok((daemon.try_send(DaemonCommand::SendSftpRequest(id)),).to_variant())
        }
        "GetPreferences" => Ok((string_map([
            ("configPath", "~/.config/desklink/protocol".to_string()),
            ("protocolVersion", "9".to_string()),
            ("network.discovery", "enabled".to_string()),
            ("clipboard.enabled", "enabled".to_string()),
            ("downloads.directory", "Downloads".to_string()),
            ("security.require-pairing", "true".to_string()),
        ]),)
            .to_variant()),
        "SetPreference" => {
            let (key, value) = parameters
                .get::<(String, String)>()
                .ok_or("Invalid SetPreference arguments")?;
            Ok((daemon.try_send(DaemonCommand::SetPreference(key, value)),).to_variant())
        }
        _ => Err(format!("Unknown method {method}")),
    }
}

pub fn start(application: &gio::Application, daemon: DaemonHandle) {
    let Some(connection) = application.dbus_connection() else {
        eprintln!("[DeskLink] GTK application D-Bus connection is unavailable");
        return;
    };
    let node = match gio::DBusNodeInfo::for_xml(INTROSPECTION) {
        Ok(node) => node,
        Err(error) => {
            eprintln!("[DeskLink] Invalid D-Bus introspection: {error}");
            return;
        }
    };
    let Some(interface) = node.lookup_interface(INTERFACE) else {
        eprintln!("[DeskLink] D-Bus interface is missing from introspection");
        return;
    };
    let method_daemon = daemon.clone();
    let registration = connection
        .register_object(OBJECT_PATH, &interface)
        .method_call(
            move |_connection,
                  _sender,
                  _object_path,
                  _interface,
                  method,
                  parameters,
                  invocation| {
                match call_method(&method_daemon, method, &parameters) {
                    Ok(value) => invocation.return_value(Some(&value)),
                    Err(error) => invocation_error(invocation, &error),
                }
            },
        )
        .build();
    if let Err(error) = registration {
        eprintln!("[DeskLink] D-Bus object registration failed: {error}");
        return;
    }
    let mut events = daemon.subscribe_events();
    let signal_connection = connection.clone();
    thread::spawn(move || {
        while let Some(event) = futures::executor::block_on(events.next()) {
            let (signal, parameters) = match event {
                CoreEvent::DeviceChanged { device } => (
                    "DeviceChanged",
                    (
                        device.id,
                        string_map([
                            ("name", device.name),
                            ("status", device.status.label().to_string()),
                            ("address", device.address),
                            ("trusted", device.trusted.to_string()),
                        ]),
                    )
                        .to_variant(),
                ),
                CoreEvent::ConnectionChanged {
                    device_id,
                    state,
                    message,
                } => (
                    "ConnectionChanged",
                    (
                        device_id,
                        string_map([
                            ("state", format!("{state:?}")),
                            ("message", message.unwrap_or_default()),
                        ]),
                    )
                        .to_variant(),
                ),
                CoreEvent::PairingChanged { device_id, state } => (
                    "PairingChanged",
                    (device_id, string_map([("state", format!("{state:?}"))])).to_variant(),
                ),
                CoreEvent::TransferChanged {
                    transfer_id,
                    state,
                    bytes_done,
                    bytes_total,
                    can_resume,
                    error,
                } => (
                    "TransferChanged",
                    (
                        transfer_id,
                        string_map([
                            ("state", state),
                            ("bytesDone", bytes_done.to_string()),
                            ("bytesTotal", bytes_total.to_string()),
                            ("canResume", can_resume.to_string()),
                            ("error", error.unwrap_or_default()),
                        ]),
                    )
                        .to_variant(),
                ),
                CoreEvent::FeatureStateChanged {
                    device_id,
                    feature,
                    state,
                    details,
                } => (
                    "FeatureStateChanged",
                    (
                        device_id,
                        feature,
                        string_map([("state", state), ("details", details.to_string())]),
                    )
                        .to_variant(),
                ),
                CoreEvent::NotificationReceived {
                    device_id,
                    notification,
                } => (
                    "NotificationReceived",
                    (
                        device_id,
                        string_map([
                            ("id", notification.id),
                            ("title", notification.title),
                            ("text", notification.text),
                        ]),
                    )
                        .to_variant(),
                ),
                CoreEvent::Error {
                    scope,
                    device_id,
                    message,
                    retryable,
                } => (
                    "Error",
                    (
                        scope,
                        string_map([
                            ("deviceId", device_id.unwrap_or_default()),
                            ("message", message),
                            ("retryable", retryable.to_string()),
                        ]),
                    )
                        .to_variant(),
                ),
            };
            if let Err(error) = signal_connection.emit_signal(
                None,
                OBJECT_PATH,
                INTERFACE,
                signal,
                Some(&parameters),
            ) {
                eprintln!("[DeskLink] Could not emit D-Bus event: {error}");
            }
        }
    });
    // GApplication owns the preserved well-known name. Registering the
    // object on that same connection avoids a second owner and keeps CLI and
    // GUI operations on one daemon.
    let _ = BUS_NAME;
}
