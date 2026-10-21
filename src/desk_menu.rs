//! The tray menu's contents and what its entries do: everything the GNOME
//! extension offers, on any desktop with a tray (KDE, Xfce, Cinnamon, Sway
//! with a bar, GNOME with an AppIndicator extension). The same actions are in
//! the application window.

use gtk::prelude::*;
use gtk::{gio, glib};

use crate::application::DesklinkApplication;
use crate::device_links::config::AppSettings;
use crate::device_links::daemon::DaemonCommand;
use crate::device_links::device::DeviceView;
use crate::device_links::transfer_log::LoggedFile;
use crate::tray::TrayEntry;

/// Recent files the menu lists.
const RECENT: usize = 5;

/// "Connected · 82% · charging", "last seen 14:02", "Connecting…".
pub(crate) fn status_words(device: &DeviceView) -> String {
    if device.feature_transport_ready {
        let mut parts = vec!["Connected".to_string()];
        if let Some(battery) = device.battery_status.as_ref() {
            parts.push(format!("{}%", battery.current_charge));
            if battery.is_charging {
                parts.push("charging".to_string());
            }
        }
        return parts.join(" · ");
    }
    if matches!(
        device.status,
        crate::device_links::device::DeviceStatus::Paired
    ) {
        return "Connecting…".to_string();
    }
    device
        .last_seen
        .and_then(|seen| glib::DateTime::from_unix_local(seen as i64).ok())
        .and_then(|seen| seen.format("last seen %H:%M").ok())
        .map(|words| words.to_string())
        .unwrap_or_else(|| "Not connected".to_string())
}

/// "Incoming call: Alice", "On a call with Alice" — no timer, so the menu
/// is not rebuilt every second.
fn call_line(status: &crate::device_links::device::CallStatus) -> String {
    use crate::device_links::device::CallState;
    let call = status.primary();
    let caller = call.caller();
    match call.state {
        CallState::Ringing if call.incoming => format!("Incoming call: {caller}"),
        CallState::Ringing | CallState::Dialing => format!("Calling {caller}…"),
        CallState::Held => format!("On hold: {caller}"),
        CallState::Talking if status.muted => format!("On a call with {caller} (muted)"),
        CallState::Talking => format!("On a call with {caller}"),
    }
}

/// The heading of a phone's section: "realme 14 5G · Connected · 82% ⚡".
fn header_words(device: &DeviceView) -> String {
    let mut words = format!("{} · ", device.name);
    if device.feature_transport_ready {
        words.push_str("Connected");
        if let Some(battery) = device
            .battery_status
            .as_ref()
            .filter(|battery| battery.current_charge >= 0)
        {
            words.push_str(&format!(" · {}%", battery.current_charge));
            if battery.is_charging {
                words.push_str(" ⚡");
            }
        }
    } else {
        words.push_str(&status_words(device));
    }
    words
}

fn device_icon(device: &DeviceView) -> &'static str {
    match device.device_type.as_str() {
        "tablet" => "tablet-symbolic",
        "laptop" | "desktop" => "computer-symbolic",
        _ => "phone-symbolic",
    }
}

/// A file's symbolic icon by its type (from the name; nothing is read): the
/// generic family ("image-x-generic"), which every icon theme has.
fn file_icon(name: &str) -> String {
    let (content_type, _) = gio::content_type_guess(Some(name), &[]);
    let generic = gio::content_type_get_generic_icon_name(&content_type)
        .map(|name| name.to_string())
        .unwrap_or_else(|| "text-x-generic".to_string());
    format!("{generic}-symbolic")
}

fn call_icon(action: &str) -> &'static str {
    match action {
        "answer" => "call-start-symbolic",
        "silence" => "audio-volume-muted-symbolic",
        "mute" => "microphone-sensitivity-muted-symbolic",
        "unmute" => "audio-input-microphone-symbolic",
        _ => "call-stop-symbolic",
    }
}

/// The tray icon: a phone using this computer, a phone connected, or none.
pub(crate) fn tray_icon(devices: &[DeviceView]) -> &'static str {
    let phones = || devices.iter().filter(|device| device.trusted);
    if phones().any(|device| crate::application::session_line(device).is_some()) {
        crate::tray::ICON_LIVE
    } else if phones().any(|device| device.feature_transport_ready) {
        crate::tray::ICON_CONNECTED
    } else {
        crate::tray::ICON_OFFLINE
    }
}

/// A connected phone's entries: its call, actions, music, notifications.
fn phone_section(phone: &DeviceView, settings: &AppSettings) -> Vec<TrayEntry> {
    let mut menu = Vec::new();
    if let Some(status) = phone.call_status.as_ref() {
        menu.push(TrayEntry::Label(call_line(status)));
        for (action, label, ..) in crate::window::calls::buttons(status) {
            menu.push(
                TrayEntry::action(label, format!("call-{action}:{}", phone.id))
                    .icon(call_icon(action)),
            );
        }
    }
    menu.push(
        TrayEntry::action("Ring", format!("ring:{}", phone.id)).icon("audio-volume-high-symbolic"),
    );
    menu.push(
        TrayEntry::action("Lock", format!("lock:{}", phone.id)).icon("system-lock-screen-symbolic"),
    );
    menu.push(
        TrayEntry::action("Send Files…", format!("send-files:{}", phone.id))
            .icon("document-send-symbolic"),
    );
    menu.push(
        TrayEntry::action("Send Clipboard", format!("send-clipboard:{}", phone.id))
            .icon("edit-paste-symbolic"),
    );
    if let Some(media) = phone
        .media_status
        .as_ref()
        .filter(|media| !media.title.is_empty())
    {
        let playing = if media.artist.is_empty() {
            media.title.clone()
        } else {
            format!("{} — {}", media.title, media.artist)
        };
        menu.push(
            TrayEntry::action(playing, format!("media-play-pause:{}", phone.id)).icon(
                if media.is_playing {
                    "media-playback-pause-symbolic"
                } else {
                    "media-playback-start-symbolic"
                },
            ),
        );
        menu.push(
            TrayEntry::action("Next Track", format!("media-next:{}", phone.id))
                .icon("media-skip-forward-symbolic"),
        );
    }
    if crate::type_on_phone::supported(phone) {
        menu.push(
            TrayEntry::action("Type on Phone…", format!("type:{}", phone.id))
                .icon("input-keyboard-symbolic"),
        );
    }
    let unread = phone.notifications.len();
    if unread > 0 && settings.shows_notifications(&phone.id) {
        menu.push(
            TrayEntry::action(
                format!("Notifications ({unread})"),
                format!("notifications:{}", phone.id),
            )
            .icon("preferences-system-notifications-symbolic"),
        );
    }
    menu
}

/// The whole menu: each phone (status, call, actions, music, notifications),
/// what is live here, this computer's switches, recent files, the app.
pub(crate) fn entries(
    devices: &[DeviceView],
    settings: &AppSettings,
    recent: &[LoggedFile],
    hand_off: bool,
) -> Vec<TrayEntry> {
    let mut menu = Vec::new();
    let phones: Vec<&DeviceView> = devices.iter().filter(|device| device.trusted).collect();
    if phones.is_empty() {
        menu.push(TrayEntry::Header {
            label: "No phone paired".into(),
            icon: "phone-symbolic".into(),
        });
        menu.push(TrayEntry::Separator);
    }
    // The first connected phone's actions are in the menu itself; any other
    // phone's in its own submenu, so two phones still fit on screen.
    let first_ready = phones
        .iter()
        .position(|phone| phone.feature_transport_ready);
    for (index, phone) in phones.iter().enumerate() {
        let header = header_words(phone);
        let icon = device_icon(phone).to_string();
        if !phone.feature_transport_ready {
            menu.push(TrayEntry::Header {
                label: header,
                icon,
            });
        } else if Some(index) == first_ready {
            menu.push(TrayEntry::Header {
                label: header,
                icon,
            });
            menu.extend(phone_section(phone, settings));
        } else {
            menu.push(TrayEntry::Submenu {
                label: header,
                children: phone_section(phone, settings),
                icon,
            });
        }
        menu.push(TrayEntry::Separator);
    }

    let live: Vec<(&DeviceView, String)> = phones
        .iter()
        .filter_map(|phone| crate::application::session_line(phone).map(|line| (*phone, line)))
        .collect();
    for (phone, line) in &live {
        menu.push(TrayEntry::Label(format!("● {line}")));
        menu.push(
            TrayEntry::action(
                format!("End {}", phone.name),
                format!("end-sessions:{}", phone.id),
            )
            .icon("process-stop-symbolic"),
        );
    }
    if live.len() > 1 {
        menu.push(
            TrayEntry::action("End Everything", "end-sessions:").icon("process-stop-symbolic"),
        );
    }
    if !live.is_empty() {
        menu.push(TrayEntry::Separator);
    }

    menu.push(TrayEntry::Toggle {
        label: "Keep Awake".into(),
        action: if settings.stay_awake {
            "keep-awake-off"
        } else {
            "keep-awake-on"
        }
        .into(),
        on: settings.stay_awake,
        icon: "weather-clear-night-symbolic".into(),
    });
    menu.push(TrayEntry::Toggle {
        label: "Clipboard Sync".into(),
        action: if settings.clipboard_sync {
            "clipboard-off"
        } else {
            "clipboard-on"
        }
        .into(),
        on: settings.clipboard_sync,
        icon: "edit-copy-symbolic".into(),
    });
    if hand_off {
        menu.push(TrayEntry::action("Lock & Hand Off", "lock-hand-off").icon("send-to-symbolic"));
    }
    if !recent.is_empty() {
        menu.push(TrayEntry::Submenu {
            label: "Recent Files".into(),
            icon: "document-open-recent-symbolic".into(),
            children: recent
                .iter()
                .take(RECENT)
                .map(|file| {
                    TrayEntry::action(
                        if file.incoming {
                            format!("{} · from {}", file.name, file.device_name)
                        } else {
                            format!("{} · to {}", file.name, file.device_name)
                        },
                        format!("open-file:{}", file.path.to_string_lossy()),
                    )
                    .icon(&file_icon(&file.name))
                })
                .collect(),
        });
    }
    menu.push(TrayEntry::Separator);
    menu.push(TrayEntry::action("Open DeskLink", "open").icon("focus-windows-symbolic"));
    menu.push(TrayEntry::action("Quit", "quit").icon("application-exit-symbolic"));
    menu
}

/// Runs a menu entry's action.
pub(crate) fn run(app: &DesklinkApplication, action: &str) {
    let (verb, target) = action.split_once(':').unwrap_or((action, ""));
    let Some(daemon) = app.daemon_handle() else {
        return;
    };
    let target = target.to_string();
    match verb {
        "ring" => daemon.send(DaemonCommand::SendFindPhone(target)),
        "call-answer" | "call-decline" | "call-silence" | "call-end" | "call-mute"
        | "call-unmute" => daemon.send(DaemonCommand::Telephony(
            target,
            verb.trim_start_matches("call-").to_string(),
            None,
        )),
        "lock" => daemon.send(DaemonCommand::SendLockRequest(target, true)),
        "type" => crate::type_on_phone::open(&daemon, &target, None),
        "send-clipboard" => daemon.send(DaemonCommand::SendClipboard(target)),
        "send-files" => {
            let name = daemon
                .devices()
                .into_iter()
                .find(|device| device.id == target)
                .map(|device| device.name)
                .unwrap_or_default();
            crate::shell_dbus::pick_and_send_files(app, &target, &name);
        }
        "notifications" => {
            app.activate();
            if let Some(window) = app.active_window().and_downcast::<crate::DesklinkWindow>() {
                window.show_device(&target);
                window.show_page("notifications");
            }
        }
        "end-sessions" => app.activate_action("end-sessions", Some(&target.to_variant())),
        "keep-awake-on" | "keep-awake-off" => {
            daemon.send(DaemonCommand::SetStayAwake(verb == "keep-awake-on"))
        }
        "clipboard-on" | "clipboard-off" => {
            daemon.send(DaemonCommand::SetClipboardSync(verb == "clipboard-on"))
        }
        "media-play-pause" | "media-next" => {
            let player = daemon
                .devices()
                .into_iter()
                .find(|device| device.id == target)
                .and_then(|device| device.media_status)
                .map(|media| media.player);
            if let Some(player) = player {
                let action = if verb == "media-next" {
                    "Next"
                } else {
                    "PlayPause"
                };
                daemon.send(DaemonCommand::SendMprisAction(
                    target,
                    player,
                    action.into(),
                ));
            }
        }
        "lock-hand-off" => app.activate_action("lock-hand-off", None),
        "open-file" => {
            // Only a file the menu offered (a recent one), never any path.
            let offered = crate::device_links::transfer_log::recent()
                .iter()
                .take(RECENT)
                .any(|file| file.path.to_string_lossy() == target);
            if offered {
                let uri = gio::File::for_path(&target).uri();
                if let Err(error) =
                    gio::AppInfo::launch_default_for_uri(&uri, gio::AppLaunchContext::NONE)
                {
                    eprintln!("[DeskLink] Could not open {target}: {error}");
                }
            }
        }
        "quit" => app.quit(),
        _ => app.activate(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device_links::device::{BatteryStatus, DeviceStatus};
    use crate::device_links::device_info::DeviceInfo;

    fn phone(id: &str, ready: bool) -> DeviceView {
        let info = DeviceInfo {
            id: id.into(),
            name: format!("Phone {id}"),
            device_type: "phone".into(),
            protocol_version: 9,
            incoming_capabilities: Vec::new(),
            outgoing_capabilities: Vec::new(),
            instance_id: None,
        };
        let mut device = DeviceView::from_info(&info, "10.0.0.2".into(), true);
        device.feature_transport_ready = ready;
        device.status = if ready {
            DeviceStatus::Paired
        } else {
            DeviceStatus::Unreachable
        };
        device
    }

    fn actions(menu: &[TrayEntry]) -> Vec<String> {
        menu.iter()
            .flat_map(|entry| match entry {
                TrayEntry::Action {
                    action,
                    enabled: true,
                    ..
                }
                | TrayEntry::Toggle { action, .. } => vec![action.clone()],
                TrayEntry::Submenu { children, .. } => actions(children),
                _ => Vec::new(),
            })
            .collect()
    }

    #[test]
    fn the_menu_offers_what_the_extension_offers() {
        let mut ready = phone("p1", true);
        ready.battery_status = Some(BatteryStatus {
            current_charge: 82,
            is_charging: true,
            battery_quantity: None,
            threshold_event: None,
            charge_time_remaining: None,
        });
        ready.terminal_sessions_attached = 1;
        ready.notifications = vec![crate::device_links::device::DeviceNotification {
            id: "n".into(),
            app_name: "Signal".into(),
            title: "Mara".into(),
            text: "hi".into(),
            ticker: String::new(),
            is_clearable: true,
            request_reply_id: Some("r".into()),
            actions: Vec::new(),
            silent: false,
        }];
        let away = phone("p2", false);
        let recent = [LoggedFile {
            ts: 1,
            device_id: "p1".into(),
            device_name: "Phone p1".into(),
            name: "a.pdf".into(),
            path: "/tmp/a.pdf".into(),
            size: 1,
            incoming: true,
        }];
        let menu = entries(&[ready, away], &AppSettings::default(), &recent, true);
        let offered = actions(&menu);
        for wanted in [
            "ring:p1",
            "lock:p1",
            "send-files:p1",
            "send-clipboard:p1",
            "notifications:p1",
            "end-sessions:p1",
            "keep-awake-on",
            "clipboard-off",
            "lock-hand-off",
            "open-file:/tmp/a.pdf",
            "open",
            "quit",
        ] {
            assert!(
                offered.iter().any(|action| action == wanted),
                "{wanted}: {offered:?}"
            );
        }
        assert!(
            !offered
                .iter()
                .any(|action| action.ends_with(":p2") && action.starts_with("ring")),
            "not for a phone that is away"
        );
        assert!(menu.contains(&TrayEntry::Header {
            label: "Phone p1 · Connected · 82% ⚡".into(),
            icon: "phone-symbolic".into(),
        }));
        assert!(menu.contains(&TrayEntry::Label("● Phone p1 — using 1 terminal".into())));
        assert_eq!(tray_icon(&[phone("p9", true)]), crate::tray::ICON_CONNECTED);
        assert_eq!(tray_icon(&[phone("p9", false)]), crate::tray::ICON_OFFLINE);
    }

    #[test]
    fn a_ringing_call_is_answered_from_the_menu_and_a_stale_one_is_not_shown() {
        use crate::device_links::device::{CallState, CallStatus, PhoneCall};
        let call = |state| CallStatus {
            calls: vec![PhoneCall {
                call_id: "c".into(),
                state,
                contact_name: Some("Alice".into()),
                number: None,
                started_at: None,
                incoming: true,
            }],
            muted: false,
            silenced: false,
        };
        let mut ringing = phone("p1", true);
        ringing.call_status = Some(call(CallState::Ringing));
        let menu = entries(&[ringing.clone()], &AppSettings::default(), &[], false);
        assert!(menu.contains(&TrayEntry::Label("Incoming call: Alice".into())));
        let offered = actions(&menu);
        for wanted in ["call-answer:p1", "call-decline:p1", "call-silence:p1"] {
            assert!(offered.iter().any(|action| action == wanted), "{wanted}");
        }
        ringing.call_status = Some(call(CallState::Talking));
        let offered = actions(&entries(
            &[ringing.clone()],
            &AppSettings::default(),
            &[],
            false,
        ));
        assert!(offered.contains(&"call-mute:p1".to_string()));
        assert!(offered.contains(&"call-end:p1".to_string()));
        ringing.feature_transport_ready = false;
        let offered = actions(&entries(&[ringing], &AppSettings::default(), &[], false));
        assert!(!offered.iter().any(|action| action.starts_with("call-")));
    }

    #[test]
    fn nothing_live_means_no_end_and_every_action_has_an_icon() {
        let menu = entries(&[phone("p1", true)], &AppSettings::default(), &[], false);
        let offered = actions(&menu);
        assert!(!offered
            .iter()
            .any(|action| action.starts_with("end-sessions")));
        assert!(!offered.contains(&"lock-hand-off".to_string()));
        assert!(menu.iter().all(|entry| match entry {
            TrayEntry::Action { icon, .. } | TrayEntry::Toggle { icon, .. } => !icon.is_empty(),
            _ => true,
        }));
        // No label twice, no two separators in a row.
        let labels: Vec<String> = menu
            .iter()
            .filter_map(|entry| match entry {
                TrayEntry::Action { label, .. } | TrayEntry::Toggle { label, .. } => {
                    Some(label.clone())
                }
                _ => None,
            })
            .collect();
        let mut unique = labels.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), labels.len(), "{labels:?}");
        assert!(!menu
            .windows(2)
            .any(|pair| pair[0] == TrayEntry::Separator && pair[1] == TrayEntry::Separator));
        assert_eq!(file_icon("notes.pdf"), "x-office-document-symbolic");
    }
}
