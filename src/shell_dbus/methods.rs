//! The `Shell1` methods: `Invoke` and friends checked and performed on the
//! application, picking files to send in DeskLink's own dialog.
//!
//! Sibling edges: `model` (actions, inserts, pages, switches). Outside:
//! `desk_features`, `pty`, `transfer_log`, `remote_control`, `window` dialogs.

use super::*;

/// One method call from a client.
pub(super) fn call(
    app: &DesklinkApplication,
    method: &str,
    params: &Variant,
) -> Result<(), String> {
    let daemon = app.daemon_handle().ok_or("DeskLink is starting")?;
    match method {
        "Invoke" => {
            let (device_id, action) = params.get::<(String, String)>().unwrap_or_default();
            let devices = current(app);
            let device = check(&devices, &device_id, &action)?;
            perform(app, device, &action);
            Ok(())
        }
        "ShowUpdate" => {
            // No arguments, nothing read from the caller, nothing started:
            // it only brings DeskLink's window (and its update popup) up.
            // With no update on offer (or no update service in this build)
            // it only brings the window up. One request keeps one wait.
            if let Some(window) = main_window(app).and_downcast::<crate::DesklinkWindow>() {
                crate::window::show_requested(&window);
            }
            Ok(())
        }
        "SeekMedia" => {
            let (device_id, position_ms) = params.get::<(String, i64)>().unwrap_or_default();
            let devices = current(app);
            check(&devices, &device_id, "media-seek")?;
            let player = daemon
                .devices()
                .into_iter()
                .find(|known| known.id == device_id)
                .and_then(|known| known.media_status)
                .map(|media| media.player)
                .ok_or("nothing is playing")?;
            daemon.send(DaemonCommand::SendMprisSetPosition(
                device_id,
                player,
                position_ms,
            ));
            Ok(())
        }
        "NotificationAction" | "NotificationReply" | "NotificationDismiss" => {
            let (device_id, notification_id, argument) = match method {
                "NotificationDismiss" => {
                    let (device, id) = params.get::<(String, String)>().unwrap_or_default();
                    (device, id, String::new())
                }
                _ => params.get::<(String, String, String)>().unwrap_or_default(),
            };
            let device = daemon
                .devices()
                .into_iter()
                .find(|device| device.id == device_id && device.trusted)
                .ok_or("no such phone")?;
            if !device.feature_transport_ready {
                return Err(format!("{} is not connected", device.name));
            }
            let notification = device
                .notifications
                .iter()
                .find(|notification| notification.id == notification_id)
                .ok_or("no such notification")?;
            match method {
                "NotificationAction" if notification.actions.contains(&argument) => daemon.send(
                    DaemonCommand::TriggerNotificationAction(device_id, notification_id, argument),
                ),
                "NotificationReply" if !argument.trim().is_empty() => {
                    let reply_id = notification
                        .request_reply_id
                        .clone()
                        .ok_or("this notification takes no reply")?;
                    daemon.send(DaemonCommand::ReplyNotification(
                        device_id, reply_id, argument,
                    ))
                }
                "NotificationDismiss" => daemon.send(DaemonCommand::DismissNotification(
                    device_id,
                    notification_id,
                )),
                _ => return Err("not an action this notification offers".to_string()),
            }
            Ok(())
        }
        "SetSwitch" => {
            let (device_id, key, on) = params.get::<(String, String, bool)>().unwrap_or_default();
            let command = resolve_switch(&key, &device_id, on)?;
            let devices = current(app);
            let device = devices.iter().find(|device| device.id == device_id);
            if !device_id.is_empty() && device.is_none() {
                return Err(format!("no device \"{device_id}\""));
            }
            switch_allowed(device, &key, on)?;
            daemon.send(command);
            Ok(())
        }
        "SendLink" => {
            let (device_id, link) = params.get::<(String, String)>().unwrap_or_default();
            let devices = current(app);
            check(&devices, &device_id, "send-link")?;
            let link = crate::window::link_to_send(&link).ok_or("not a link")?;
            daemon.send(DaemonCommand::SendShareText(device_id, link));
            Ok(())
        }
        "OpenTransfer" => {
            let id = params.get::<(String,)>().unwrap_or_default().0;
            // Only a file on the recent list, never an arbitrary path.
            let file = crate::device_links::transfer_log::recent()
                .into_iter()
                .take(RECENT_TRANSFERS)
                .find(|file| file.path.to_string_lossy() == id)
                .ok_or("not a recent transfer")?;
            let uri = gio::File::for_path(&file.path).uri();
            gio::AppInfo::launch_default_for_uri(&uri, gio::AppLaunchContext::NONE)
                .map_err(|error| error.to_string())
        }
        _ => Err("unknown method".to_string()),
    }
}

fn perform(app: &DesklinkApplication, device: Option<&ShellDevice>, action: &str) {
    let id = device.map(|device| device.id.clone());
    let daemon = app.daemon_handle();
    match (action, id, daemon) {
        ("ring", Some(id), Some(daemon)) => daemon.send(DaemonCommand::SendFindPhone(id)),
        ("lock", Some(id), Some(daemon)) => daemon.send(DaemonCommand::SendLockRequest(id, true)),
        ("ping", Some(id), Some(daemon)) => daemon.send(DaemonCommand::SendPing(id)),
        ("media-prev" | "media-play-pause" | "media-next", Some(id), Some(daemon)) => {
            let player = daemon
                .devices()
                .into_iter()
                .find(|known| known.id == id)
                .and_then(|known| known.media_status)
                .map(|media| media.player);
            if let Some(player) = player {
                let verb = match action {
                    "media-next" => "Next",
                    "media-prev" => "Previous",
                    _ => "PlayPause",
                };
                daemon.send(DaemonCommand::SendMprisAction(id, player, verb.into()));
            }
        }
        ("send-clipboard", Some(id), Some(daemon)) => daemon.send(DaemonCommand::SendClipboard(id)),
        (
            "call-answer" | "call-decline" | "call-silence" | "call-end" | "call-mute"
            | "call-unmute",
            Some(id),
            Some(daemon),
        ) => daemon.send(DaemonCommand::Telephony(
            id,
            action.trim_start_matches("call-").to_string(),
            None,
        )),
        ("try-again", Some(id), Some(daemon)) => daemon.send(DaemonCommand::Reconnect(id)),
        ("reconnect", _, Some(daemon)) => {
            daemon.send(DaemonCommand::NetworkChanged);
            daemon.send(DaemonCommand::Discover);
        }
        ("end-sessions", id, Some(daemon)) => daemon.send(DaemonCommand::EndPhoneSessions(id)),
        ("lock-hand-off", _, Some(daemon)) => daemon.send(DaemonCommand::LockAndHandOff),
        ("keep-awake-on" | "keep-awake-off", _, Some(daemon)) => {
            daemon.send(DaemonCommand::SetStayAwake(action == "keep-awake-on"))
        }
        ("clipboard-on" | "clipboard-off", _, Some(daemon)) => {
            daemon.send(DaemonCommand::SetClipboardSync(action == "clipboard-on"))
        }
        ("unlock-via-phone", Some(id), _) => {
            // Reads the lock state (loginctl): off the main loop. Every
            // check is made again there and when the phone answers.
            std::thread::spawn(move || {
                crate::device_links::desk_features::offer_unlock(&id, "button");
            });
        }
        ("new-terminal", _, _) => {
            if let Err(error) = crate::device_links::pty::desk::new_desk_session() {
                eprintln!("[Shell] Could not start a terminal: {error}");
            }
        }
        ("send-files", Some(id), Some(_)) => {
            let name = device
                .map(|device| device.name.as_str())
                .unwrap_or_default();
            pick_and_send_files(app, &id, name);
        }
        (insert, Some(id), Some(daemon)) if insert.starts_with("insert-") => {
            let capture = INSERTS
                .iter()
                .find(|(action, _)| *action == insert)
                .and_then(|(_, index)| crate::window::insert::CAPTURES.get(*index));
            if let Some((kind, target, ..)) = capture {
                daemon.send(DaemonCommand::Capture(id, (*kind).into(), (*target).into()));
            }
        }
        ("show-screen", Some(id), Some(daemon)) => {
            // The phone still has to agree to share; the dialog says so.
            if let Some(window) = main_window(app) {
                crate::remote_control::show_remote_control_dialog(daemon, id, &window);
            }
        }
        ("unpair", Some(id), Some(daemon)) => {
            // The window's own question, never a silent unpairing.
            if let Some(window) = main_window(app).and_downcast::<crate::DesklinkWindow>() {
                let name = device.map(|device| device.name.clone()).unwrap_or_default();
                window.show_device(&id);
                crate::window::confirm(
                    &window,
                    &format!("Unpair {name}?"),
                    "It can connect again only after pairing again.",
                    "_Unpair",
                    move || daemon.send(DaemonCommand::Unpair(id.clone())),
                );
            }
        }
        (page, Some(id), _) if PAGES.iter().any(|(action, _)| *action == page) => {
            if let (Some(window), Some((_, name))) = (
                main_window(app).and_downcast::<crate::DesklinkWindow>(),
                PAGES.iter().find(|(action, _)| *action == page),
            ) {
                window.show_device(&id);
                window.show_page(name);
            }
        }
        ("open-bucket", Some(id), _) => {
            app.activate_action("open-bucket", Some(&id.to_variant()));
        }
        ("new-note", _, _) => {
            let window = app.main_window();
            window.present();
            window.new_note();
        }
        ("pair-new", _, _) => {
            if let Some(window) = main_window(app) {
                let _ = window.activate_action("win.pair-new", None);
            }
        }
        // "open"
        _ => {
            let window = main_window(app);
            if let (Some(window), Some(device)) =
                (window.and_downcast::<crate::DesklinkWindow>(), device)
            {
                window.show_device(&device.id);
            }
        }
    }
}

/// Asks for files in DeskLink's own dialog and sends them to `device_id`.
pub(crate) fn pick_and_send_files(app: &DesklinkApplication, device_id: &str, name: &str) {
    let Some(daemon) = app.daemon_handle() else {
        return;
    };
    let window = main_window(app);
    let id = device_id.to_string();
    crate::window::send_files_dialog(&format!("Send Files to {name}")).open_multiple(
        window.as_ref(),
        gio::Cancellable::NONE,
        move |files| {
            let Ok(files) = files else { return };
            crate::window::remember_send_folder(&files);
            for file in files.iter::<gio::File>().flatten() {
                if let Some(path) = file.path() {
                    daemon.send(DaemonCommand::SendFile(id.clone(), path));
                }
            }
        },
    );
}

/// Presents the main window (creating it if needed) and returns it.
fn main_window(app: &DesklinkApplication) -> Option<gtk::Window> {
    app.activate();
    // Never the Phone Screen window, which is the app's own window too.
    Some(app.main_window().upcast())
}
