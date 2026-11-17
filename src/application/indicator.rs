//! The session indicator (`impl DesklinkApplication`): the once-a-second
//! poll that turns daemon events into notifications, approvals, held links,
//! Glide and Bucket questions, walk-away locking and transfer notices.
//!
//! Sibling edges: none (the parent's helpers). Outside: `device_links`
//! (`daemon`, `asks`, `config`, `device`, `dnd`, `folders`, `glide`,
//! `transfer_log`, `walk_away`, `webrtc`), `desk_menu`, `tray`, `shell_dbus`,
//! `terminal_previews`, `ticker`, `transfer_notice`, `crate::guard`.

use super::*;

impl DesklinkApplication {
    /// Shows what phones are doing here — in the top bar when a tray host
    /// exists, otherwise as a notification when a session starts — once a
    /// second, from the daemon's device view.
    pub(super) fn start_session_indicator(&self) {
        let app = self.downgrade();
        let actions = crate::tray::TrayActions {
            open: Box::new({
                let app = app.clone();
                move || {
                    if let Some(app) = app.upgrade() {
                        app.activate();
                    }
                }
            }),
            run: Box::new({
                let app = app.clone();
                move |action| {
                    if let Some(app) = app.upgrade() {
                        crate::desk_menu::run(&app, action);
                    }
                }
            }),
        };
        match crate::tray::Tray::new(actions) {
            Ok(tray) => {
                self.imp().tray.replace(Some(tray));
            }
            Err(error) => eprintln!("[DeskLink] No top-bar indicator: {error}"),
        }

        let mut notified = std::collections::HashSet::<String>::new();
        let mut asked = std::collections::HashSet::<String>::new();
        let mut approving = std::collections::HashSet::<String>::new();
        let mut glide_asking = std::collections::HashSet::<String>::new();
        let mut notices = crate::transfer_notice::Notices::default();
        crate::ticker::every_tick(move || {
            crate::guard("app tick", glib::ControlFlow::Continue, || {
                let Some(app) = app.upgrade() else {
                    return glib::ControlFlow::Break;
                };
                let Some(daemon) = app.daemon_handle() else {
                    return glib::ControlFlow::Continue;
                };
                let active: Vec<_> = daemon
                    .devices()
                    .into_iter()
                    .filter_map(|device| session_line(&device).map(|line| (device, line)))
                    .collect();
                let hosted = app
                    .imp()
                    .tray
                    .borrow()
                    .as_ref()
                    .is_some_and(|tray| tray.hosted())
                    || crate::shell_dbus::extension_active();
                if !hosted {
                    for (device, line) in &active {
                        if notified.insert(device.id.clone()) {
                            let notification = crate::application::desk_notification(line);
                            notification.set_body(Some("Open DeskLink to see more."));
                            notification.add_button_with_target_value(
                                "End session",
                                "app.end-sessions",
                                Some(&device.id.to_variant()),
                            );
                            app.send_notification(
                                Some(&format!("session-{}", device.id)),
                                &notification,
                            );
                        }
                    }
                }
                notified.retain(|id| active.iter().any(|(device, _)| &device.id == id));
                // A phone that asks for a Bucket is answered with one click
                // on a notification, once per ask (turning it on stays the
                // person's choice).
                for offer in
                    new_bucket_questions(&crate::device_links::folders::offers(), &mut asked)
                {
                    let phone = daemon
                        .devices()
                        .into_iter()
                        .find(|device| device.id == offer.device)
                        .map_or(offer.name.clone(), |device| device.name);
                    let notification = crate::application::desk_notification(&format!(
                        "{phone} wants to share a Bucket"
                    ));
                    notification.set_body(Some(
                        "One place for files, pictures, text and links on the phone and the computer.",
                    ));
                    notification.add_button_with_target_value(
                        "Turn on",
                        "app.bucket-turn-on",
                        Some(&offer.device.to_variant()),
                    );
                    crate::application::open_on_click(
                        &notification,
                        "app.open-bucket",
                        Some(&offer.device.to_variant()),
                    );
                    app.send_notification(
                        Some(&format!("bucket-ask-{}", offer.device)),
                        &notification,
                    );
                }
                // A phone's screen request waits for an answer in GNOME's own dialog,
                // which says nothing about DeskLink: say where to look, and take
                // the notice down the moment it is answered.
                let waiting = crate::device_links::webrtc::coordinator::phones_awaiting_approval();
                let (to_post, to_withdraw) = approval_changes(&waiting, &mut approving);
                for device in to_post {
                    let phone = daemon
                        .devices()
                        .into_iter()
                        .find(|known| known.id == device)
                        .map_or_else(|| "A phone".to_string(), |known| known.name);
                    let notification =
                        gio::Notification::new(&format!("{phone} wants to show this screen"));
                    notification.set_body(Some("Approve it in the window that just opened."));
                    notification.set_priority(gio::NotificationPriority::High);
                    notification.set_icon(&gio::ThemedIcon::new("derx06.desklink.com-symbolic"));
                    app.send_notification(Some(&format!("screen-approve-{device}")), &notification);
                }
                for device in to_withdraw {
                    app.withdraw_notification(&format!("screen-approve-{device}"));
                }
                // A share that could not start: said here as well as on the phone.
                for (device, reason) in
                    crate::device_links::webrtc::coordinator::take_share_failures()
                {
                    let phone = daemon
                        .devices()
                        .into_iter()
                        .find(|known| known.id == device)
                        .map_or_else(|| "the phone".to_string(), |known| known.name);
                    let notification = gio::Notification::new(&format!(
                        "This screen couldn't be shown on {phone}"
                    ));
                    notification.set_body(Some(&reason));
                    notification.set_priority(gio::NotificationPriority::High);
                    notification.set_icon(&gio::ThemedIcon::new("derx06.desklink.com-symbolic"));
                    app.send_notification(Some(&format!("screen-failed-{device}")), &notification);
                }
                // Chrome (or an Electron app) was open when accessibility came
                // on: it cannot report text fields until restarted. Said once.
                if crate::device_links::webrtc::text_focus::take_restart_hint() {
                    let notification =
                        gio::Notification::new("Restart Chrome once for the phone keyboard");
                    notification.set_body(Some(
                        "Chrome and apps like VS Code that were already open can't tell DeskLink \
                         when you click a text field. Restart them once and the phone keyboard \
                         comes up for their text fields too.",
                    ));
                    notification.set_icon(&gio::ThemedIcon::new("derx06.desklink.com-symbolic"));
                    app.send_notification(Some("text-focus-restart"), &notification);
                }
                // Glide is about to put GNOME's "Capture Input" dialog up: say
                // so, and take the notice down the moment it is answered.
                let glide = crate::device_links::glide::status();
                let (to_post, to_withdraw) = approval_changes(
                    &glide_waiting(
                        glide.asking,
                        crate::device_links::config::app_settings()
                            .glide
                            .map(|setting| setting.device),
                    ),
                    &mut glide_asking,
                );
                for device in to_post {
                    let phone = daemon
                        .devices()
                        .into_iter()
                        .find(|known| known.id == device)
                        .map_or_else(|| "your phone".to_string(), |known| known.name);
                    let notification = gio::Notification::new(&format!(
                        "DeskLink is asking to move your mouse to {phone} (Glide)"
                    ));
                    notification.set_body(Some("Approve it in the window that just opened."));
                    notification.set_priority(gio::NotificationPriority::High);
                    notification.set_icon(&gio::ThemedIcon::new("derx06.desklink.com-symbolic"));
                    app.send_notification(Some(&format!("glide-ask-{device}")), &notification);
                }
                for device in to_withdraw {
                    app.withdraw_notification(&format!("glide-ask-{device}"));
                }
                {
                    let devices = daemon.devices();
                    app.walk_away_step(&devices);
                    app.transfer_notices(&mut notices, &devices);
                    let settings = crate::device_links::config::app_settings();
                    let paired = || devices.iter().filter(|device| device.trusted);
                    if let Some(problem) = crate::device_links::dnd::step(
                        &paired()
                            .filter(|d| settings.shares_dnd_with(&d.id))
                            .map(|d| d.id.clone())
                            .collect(),
                        &paired()
                            .filter(|d| d.feature_transport_ready)
                            .map(|d| d.id.clone())
                            .collect(),
                    ) {
                        let notification =
                            crate::application::desk_notification("Do Not Disturb was not shared");
                        notification.set_body(Some(&problem));
                        app.send_notification(Some("dnd-problem"), &notification);
                    }
                }
                // Asks from a phone (Ask on computer): a notification with
                // Allow and Not now, once per ask, withdrawn when answered.
                if let Some(bus) = app.dbus_connection() {
                    crate::device_links::asks::probe_buttons(&bus);
                }
                let (new_asks, gone_asks) = crate::device_links::asks::notices();
                for id in gone_asks {
                    app.withdraw_notification(&id);
                }
                for ask in new_asks {
                    let phone = daemon
                        .devices()
                        .into_iter()
                        .find(|device| device.id == ask.device)
                        .map_or_else(|| "Your phone".to_string(), |device| device.name);
                    let Some(title) = crate::device_links::asks::prompt(&phone, ask.key) else {
                        continue;
                    };
                    let notification = crate::application::desk_notification(&title);
                    notification.set_body(Some("Allow turns it on. Nothing changes until you do."));
                    notification.set_priority(gio::NotificationPriority::High);
                    for (label, allow) in [("Allow", true), ("Not now", false)] {
                        notification.add_button_with_target_value(
                            label,
                            "app.ask-answer",
                            Some(&(ask.device.as_str(), ask.key, allow).to_variant()),
                        );
                    }
                    crate::application::open_on_click(&notification, "app.show-window", None);
                    app.send_notification(
                        Some(&crate::device_links::asks::notice_id(&ask.device, ask.key)),
                        &notification,
                    );
                }
                if let Some(tray) = app.imp().tray.borrow().as_ref() {
                    let devices = daemon.devices();
                    let hand_off = devices.iter().any(|device| device.feature_transport_ready)
                        && crate::terminal_previews::sessions()
                            .iter()
                            .any(|info| info.exit_status.is_none());
                    let menu = crate::desk_menu::entries(
                        &devices,
                        &crate::device_links::config::app_settings(),
                        &crate::device_links::transfer_log::recent(),
                        hand_off,
                    );
                    let icon = crate::desk_menu::tray_icon(&devices);
                    // A waiting ask is a line too: the indicator asks for
                    // attention (the badge) while the window is closed.
                    let mut lines: Vec<String> = active.into_iter().map(|(_, line)| line).collect();
                    lines.extend(crate::desk_menu::ask_lines(&devices));
                    tray.update(
                        lines,
                        crate::desk_menu::tooltip(&devices),
                        menu,
                        icon,
                        // DeskLink's own extension is in the top bar already.
                        crate::shell_dbus::extension_active(),
                    );
                }
                glib::ControlFlow::Continue
            })
        });
    }

    /// Walk-away lock: the look at every phone, and what it asks for (a
    /// warning with Cancel, then the lock). See `walk_away`.
    fn walk_away_step(&self, devices: &[crate::device_links::device::DeviceView]) {
        use crate::device_links::walk_away::{network_is_up, observe, Seen, Step};
        let settings = crate::device_links::config::app_settings();
        for device in devices.iter().filter(|device| device.trusted) {
            let seen = Seen::of(
                device,
                settings.locks_when_away(&device.id),
                crate::device_links::webrtc::coordinator::is_dozing(&device.id),
                network_is_up(gio::NetworkMonitor::default().connectivity()),
            );
            let id = format!("walk-away-{}", device.id);
            match observe(&device.id, seen) {
                Some(Step::Warn) => {
                    let notification = crate::application::desk_notification(
                        "Locking this computer in 10 seconds",
                    );
                    notification.set_body(Some(&format!(
                        "{} has been out of reach. Cancel to stay unlocked.",
                        glib::markup_escape_text(&device.name)
                    )));
                    notification.set_priority(gio::NotificationPriority::Urgent);
                    notification.add_button_with_target_value(
                        "Cancel",
                        "app.walk-away-cancel",
                        Some(&device.id.to_variant()),
                    );
                    self.send_notification(Some(&id), &notification);
                }
                Some(Step::ClearWarning) => self.withdraw_notification(&id),
                Some(Step::Lock) => {
                    self.withdraw_notification(&id);
                    // D-Bus with a timeout: not on the main loop.
                    let _ = std::thread::Builder::new()
                        .name("desklink-walk-away".into())
                        .spawn(crate::device_links::daemon::packet_handler::lock_desktop);
                }
                None => {}
            }
        }
    }

    /// The "DeskLink" notification for files moving (see `transfer_notice`):
    /// progress while they move, how they ended after.
    fn transfer_notices(
        &self,
        notices: &mut crate::transfer_notice::Notices,
        devices: &[crate::device_links::device::DeviceView],
    ) {
        use crate::device_links::webrtc::transfer_board as board;
        use crate::transfer_notice::Action;
        // Together, under one lock; the Bucket's own sync is not here (its
        // module announces it).
        let (rows, news, seen) = board::snapshot(notices.seen());
        notices.set_seen(seen);
        notices.set_lanes(board::lanes());
        let name_of = |id: &str| {
            devices
                .iter()
                .find(|device| device.id == id)
                .map_or_else(|| "your phone".to_string(), |device| device.name.clone())
        };
        let actions = notices.look(std::time::Instant::now(), &rows, &news, &name_of, &|id| {
            board::note(id)
        });
        // Do Not Disturb on: the desktop keeps banners back, and a progress
        // line every few seconds is not worth interrupting for.
        let quiet = crate::device_links::dnd::desk_state() == Some(true);
        for action in actions {
            match action {
                Action::Progress { device, body } if !quiet => {
                    let notification = gio::Notification::new("DeskLink");
                    notification.set_body(Some(&body));
                    notification.set_priority(gio::NotificationPriority::Low);
                    notification.set_icon(&gio::ThemedIcon::new("derx06.desklink.com-symbolic"));
                    notification.add_button_with_target_value(
                        "Cancel",
                        "app.cancel-transfers",
                        Some(&device.to_variant()),
                    );
                    crate::application::open_on_click(
                        &notification,
                        "app.open-bucket",
                        Some(&device.to_variant()),
                    );
                    self.send_notification(Some(&format!("transfers-{device}")), &notification);
                }
                Action::Progress { .. } => {}
                Action::Ended { .. } if quiet => {}
                Action::Ended {
                    device,
                    title,
                    path,
                } => {
                    let notification = gio::Notification::new("DeskLink");
                    notification.set_body(Some(&title));
                    notification.set_icon(&gio::ThemedIcon::new("derx06.desklink.com-symbolic"));
                    crate::application::open_on_click(
                        &notification,
                        "app.open-bucket",
                        Some(&device.to_variant()),
                    );
                    if let Some(path) = path {
                        let target = path.to_string_lossy().to_variant();
                        notification.add_button_with_target_value(
                            "Open",
                            "app.open-received",
                            Some(&target),
                        );
                        notification.add_button_with_target_value(
                            "Show in Files",
                            "app.show-received",
                            Some(&target),
                        );
                    }
                    self.send_notification(Some(&format!("transfers-{device}")), &notification);
                }
                Action::Clear { device } => {
                    self.withdraw_notification(&format!("transfers-{device}"));
                }
            }
        }
    }
}
