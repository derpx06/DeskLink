use std::collections::HashMap;

use crate::branding::PRODUCT_NAME;
use crate::device_links::daemon::DaemonHandle;
use gtk::glib;

pub fn run() -> Option<glib::ExitCode> {
    let mut args = std::env::args().skip(1);
    let first = args.next();
    match first.as_deref() {
        Some("--version") => {
            println!("{PRODUCT_NAME} {}", env!("CARGO_PKG_VERSION"));
            Some(glib::ExitCode::SUCCESS)
        }
        // GApplication consumes this flag for D-Bus activation.  It must not
        // be interpreted as a CLI command before GTK gets the arguments.
        Some("--gapplication-service") => None,
        Some("--list") => run_legacy_list(),
        Some("--daemon") | Some("daemon") => run_daemon(),
        Some("devices") => {
            if args.next().as_deref() != Some("list") {
                return command_error("usage: desklink devices list [--json]");
            }
            let json = args.next().as_deref() == Some("--json");
            with_client(|client| {
                let devices = client.list_devices().map_err(|error| error.to_string())?;
                if json {
                    println!(
                        "{}",
                        serde_json::to_string(&devices).map_err(|error| error.to_string())?
                    );
                } else if devices.is_empty() {
                    println!("No DeskLink devices discovered.");
                } else {
                    for device in devices {
                        println!("{device:?}");
                    }
                }
                Ok(())
            })
        }
        Some("device") => run_device_command(&mut args),
        Some("share") => run_share_command(&mut args),
        Some("clipboard") => run_clipboard_command(&mut args),
        Some("transfer") => run_transfer_command(&mut args),
        Some("sftp") => {
            if args.next().as_deref() != Some("open") {
                return command_error("usage: desklink sftp open <device-id>");
            }
            let Some(device_id) = args.next() else {
                return command_error("usage: desklink sftp open <device-id>");
            };
            with_client(|client| {
                client
                    .browse_sftp(&device_id)
                    .map(|accepted| println!("{accepted}"))
                    .map_err(|error| error.to_string())
            })
        }
        Some("feature") => run_feature_command(&mut args),
        Some("preferences") => run_preferences_command(&mut args),
        Some(other) => command_error(&format!("unknown command: {other}")),
        None => None,
    }
}

fn run_legacy_list() -> Option<glib::ExitCode> {
    with_client(|client| {
        let devices = client.list_devices().map_err(|error| error.to_string())?;
        for device in devices {
            println!("{device:?}");
        }
        Ok(())
    })
}

fn run_daemon() -> Option<glib::ExitCode> {
    // Prefer the already-activated service.  Starting another in-process
    // daemon would create a second discovery socket and duplicate sessions.
    if let Ok(client) = crate::dbus::client::DeskLinkClient::connect() {
        if client.list_devices().is_ok() {
            eprintln!("{PRODUCT_NAME} is already running");
            return Some(glib::ExitCode::SUCCESS);
        }
    }
    let daemon = DaemonHandle::start();
    crate::dbus::start_headless(daemon);
    loop {
        std::thread::park();
    }
}

fn run_device_command(args: &mut impl Iterator<Item = String>) -> Option<glib::ExitCode> {
    let Some(action) = args.next() else {
        return command_error("usage: desklink device <pair|unpair|ping> <device-id>");
    };
    let Some(device_id) = args.next() else {
        return command_error("missing device-id");
    };
    with_client(|client| {
        let result = match action.as_str() {
            "pair" => client.pair(&device_id),
            "unpair" => client.unpair(&device_id),
            "ping" => client.ping(&device_id),
            _ => return Err(format!("unknown device action: {action}")),
        };
        println!("{}", result.map_err(|error| error.to_string())?);
        Ok(())
    })
}

fn run_share_command(args: &mut impl Iterator<Item = String>) -> Option<glib::ExitCode> {
    let Some(action) = args.next() else {
        return command_error("usage: desklink share <file|url> <device-id> <value...>");
    };
    let Some(device_id) = args.next() else {
        return command_error("missing device-id");
    };
    let values: Vec<_> = args.collect();
    if values.is_empty() {
        return command_error("missing share value");
    }
    with_client(|client| match action.as_str() {
        "file" => client
            .share_files(&device_id, &values)
            .map(|accepted| println!("{accepted}"))
            .map_err(|error| error.to_string()),
        "url" => client
            .share_url(&device_id, &values[0])
            .map(|accepted| println!("{accepted}"))
            .map_err(|error| error.to_string()),
        _ => Err(format!("unknown share action: {action}")),
    })
}

fn run_clipboard_command(args: &mut impl Iterator<Item = String>) -> Option<glib::ExitCode> {
    if args.next().as_deref() != Some("set") {
        return command_error("usage: desklink clipboard set <device-id> <text>");
    }
    let Some(device_id) = args.next() else {
        return command_error("missing device-id");
    };
    let text = args.collect::<Vec<_>>().join(" ");
    if text.is_empty() {
        return command_error("missing clipboard text");
    }
    with_client(|client| {
        client
            .set_clipboard(&device_id, &text)
            .map(|accepted| println!("{accepted}"))
            .map_err(|error| error.to_string())
    })
}

fn run_transfer_command(args: &mut impl Iterator<Item = String>) -> Option<glib::ExitCode> {
    let Some(action) = args.next() else {
        return command_error("usage: desklink transfer <start|cancel> ...");
    };
    let Some(value) = args.next() else {
        return command_error("missing transfer argument");
    };
    with_client(|client| match action.as_str() {
        "start" => {
            let Some(path) = args.next() else {
                return Err("usage: desklink transfer start <device-id> <path>".to_string());
            };
            client
                .start_transfer(&value, &path)
                .map(|transfer_id| println!("{transfer_id}"))
                .map_err(|error| error.to_string())
        }
        "status" => client
            .get_transfer(&value)
            .map(|status| println!("{status:?}"))
            .map_err(|error| error.to_string()),
        "cancel" => client
            .cancel_transfer(&value)
            .map(|cancelled| println!("{cancelled}"))
            .map_err(|error| error.to_string()),
        _ => Err(format!("unknown transfer action: {action}")),
    })
}

fn run_feature_command(args: &mut impl Iterator<Item = String>) -> Option<glib::ExitCode> {
    if args.next().as_deref() != Some("invoke") {
        return command_error("usage: desklink feature invoke <device-id> <action> [key=value...]");
    }
    let Some(device_id) = args.next() else {
        return command_error("missing device-id");
    };
    let Some(action) = args.next() else {
        return command_error("missing feature action");
    };
    let arguments = args
        .filter_map(|argument| {
            argument
                .split_once('=')
                .map(|(key, value)| (key.to_string(), value.to_string()))
        })
        .collect::<HashMap<_, _>>();
    with_client(|client| {
        client
            .invoke_feature_action(&device_id, &action, &arguments)
            .map(|accepted| println!("{accepted}"))
            .map_err(|error| error.to_string())
    })
}

fn run_preferences_command(args: &mut impl Iterator<Item = String>) -> Option<glib::ExitCode> {
    let Some(action) = args.next() else {
        return command_error("usage: desklink preferences <get|set> ...");
    };
    with_client(|client| match action.as_str() {
        "get" => {
            let preferences = client
                .get_preferences()
                .map_err(|error| error.to_string())?;
            if args.next().as_deref() == Some("--json") {
                println!(
                    "{}",
                    serde_json::to_string(&preferences).map_err(|error| error.to_string())?
                );
            } else {
                for (key, value) in preferences {
                    println!("{key}={value}");
                }
            }
            Ok(())
        }
        "set" => {
            let Some(key) = args.next() else {
                return Err("missing preference key".to_string());
            };
            let Some(value) = args.next() else {
                return Err("missing preference value".to_string());
            };
            println!(
                "{}",
                client
                    .set_preference(&key, &value)
                    .map_err(|error| error.to_string())?
            );
            Ok(())
        }
        _ => Err(format!("unknown preferences action: {action}")),
    })
}

fn with_client<F>(operation: F) -> Option<glib::ExitCode>
where
    F: FnOnce(&crate::dbus::client::DeskLinkClient) -> Result<(), String>,
{
    match crate::dbus::client::DeskLinkClient::connect()
        .and_then(|client| operation(&client).map_err(zbus::Error::Failure))
    {
        Ok(()) => Some(glib::ExitCode::SUCCESS),
        Err(error) => {
            eprintln!("{PRODUCT_NAME} command failed: {error}");
            Some(glib::ExitCode::FAILURE)
        }
    }
}

fn command_error(message: &str) -> Option<glib::ExitCode> {
    eprintln!("{message}");
    Some(glib::ExitCode::FAILURE)
}
