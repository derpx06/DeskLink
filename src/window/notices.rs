//! What the window tells the person (`impl DesklinkWindow`): effects of a
//! device's state, quick actions, app notifications, finished transfers,
//! sticky notices, errors and their details, Wi-Fi offers, links and toasts.
//!
//! Sibling edges: `imp`, `words`.

use super::*;

impl DesklinkWindow {
    /// Pairing dialogs, mirrored phone notifications and the error banner.
    ///
    /// Driven by every known device, not by the page on show: a pair request
    /// or a notification from a device not selected must still be seen.
    pub(super) fn mirror_device_side_effects(
        &self,
        daemon: &DaemonHandle,
        devices: &[DeviceView],
        errors_empty: bool,
    ) {
        let imp = self.imp();

        let current_pair_requests = devices
            .iter()
            .filter(|device| matches!(device.status, DeviceStatus::PairRequestedByPeer))
            .map(|device| device.id.clone())
            .collect::<std::collections::HashSet<_>>();
        imp.notified_pair_requests
            .borrow_mut()
            .retain(|device_id| current_pair_requests.contains(device_id));
        // A request that ended (accepted on the phone, timed out) or whose code
        // changed takes its dialog with it.
        let stale: Vec<(String, adw::AlertDialog)> = {
            let mut dialogs = imp.pair_dialogs.borrow_mut();
            let ids: Vec<String> = dialogs
                .iter()
                .filter(|(id, (key, _))| {
                    !devices.iter().any(|device| {
                        &device.id == *id
                            && matches!(device.status, DeviceStatus::PairRequestedByPeer)
                            && &device.verification_key == key
                    })
                })
                .map(|(id, _)| id.clone())
                .collect();
            ids.into_iter()
                .filter_map(|id| dialogs.remove(&id).map(|(_, dialog)| (id, dialog)))
                .collect()
        };
        // Requests still open whose code changed: shown again with the new code.
        let mut reopen = std::collections::HashSet::new();
        for (id, dialog) in stale {
            dialog.force_close();
            reopen.insert(id);
        }
        for device in devices
            .iter()
            .filter(|device| matches!(device.status, DeviceStatus::PairRequestedByPeer))
        {
            // The borrow is resolved to a bool before the notification call.
            // Holding a `RefMut` across `send_app_notification` risked a panic
            // if GIO ever pumped the main loop and re-entered this timeout.
            let is_new = imp
                .notified_pair_requests
                .borrow_mut()
                .insert(device.id.clone());
            if is_new {
                let body = device
                    .verification_key
                    .as_ref()
                    .map(|key| format!("Verification code: {}", spaced_code(key)))
                    .unwrap_or_else(|| {
                        "Open DeskLink to accept or reject the request.".to_string()
                    });
                self.send_app_notification(
                    &format!("pair-request-{}", device.id),
                    &format!("Pairing request from {}", device.name),
                    &body,
                );
            }
            // A dismissed dialog stays dismissed until the code changes; the
            // device's overview still offers Pair and Reject.
            if is_new || reopen.contains(&device.id) {
                pairing::present_pair_dialog(self, daemon, device);
            }
        }

        // A lasting state (offline) keeps its banner a while after its last
        // report, so a once-a-cycle error does not make it flicker.
        let banner_settled = imp
            .banner_since
            .get()
            .is_none_or(|since| since.elapsed() >= std::time::Duration::from_secs(10));
        // A standing problem (no WebRTC plugins, nothing can be saved) is
        // asserted again at every poll, so it outlives the banner timer.
        let sticky = self.assert_sticky_notice();
        if !sticky
            && errors_empty
            && banner_settled
            && devices.iter().all(|device| device.last_error.is_none())
        {
            imp.error_banner.set_revealed(false);
        }

        let settings = crate::device_links::config::app_settings();
        let announce = imp.notified_remote_notifications.borrow_mut().pass(
            devices,
            &|id| settings.shows_notifications(id),
            crate::shell_dbus::client_handles("notifications"),
            std::time::Instant::now(),
        );
        for (device, notification) in announce {
            let unique_id = format!("{}:{}", device.id, notification.id);
            let title = if notification.title.is_empty() {
                format!("{} on {}", notification.app_name, device.name)
            } else {
                format!("{}: {}", notification.app_name, notification.title)
            };
            // One paragraph: the desktop's banner and the toast cap lines
            // per paragraph, or not at all.
            let body =
                super::notifications::one_paragraph(super::notifications::body(notification));
            // In front of the user: a toast; otherwise the desktop's own.
            if self.is_active() && self.is_visible() {
                let toast = adw::Toast::new(&banner_line(&format!("{title} — {body}")));
                toast.set_use_markup(false);
                toast.set_timeout(5);
                toast.set_button_label(Some("View"));
                toast.set_action_name(Some("win.show-page"));
                toast.set_action_target_value(Some(&"notifications".to_variant()));
                self.queue_toast(&toast);
            } else {
                self.send_phone_notification(
                    &format!("phone-notification-{unique_id}"),
                    &title,
                    &body,
                    notification.app_icon.as_deref(),
                );
            }
        }
    }

    pub(super) fn quick_action(&self, action: QuickAction) {
        let Some(daemon) = self.imp().daemon.borrow().clone() else {
            return;
        };
        let Some(id) = self.imp().selected_device_id.borrow().clone() else {
            return;
        };
        let name = daemon
            .devices()
            .into_iter()
            .find(|device| device.id == id)
            .map(|device| device.name)
            .unwrap_or_default();
        let (command, message) = match action {
            QuickAction::Ring => (
                DaemonCommand::SendFindPhone(id),
                format!("Asking {name} to ring"),
            ),
            QuickAction::Lock => (
                DaemonCommand::SendLockRequest(id, true),
                format!("Asking {name} to lock"),
            ),
            QuickAction::Ping => (DaemonCommand::SendPing(id), format!("Nudging {name}")),
            QuickAction::Clipboard => (
                DaemonCommand::SendClipboard(id),
                format!("Sending the clipboard to {name}"),
            ),
        };
        daemon.send(command);
        self.show_feature_toast(&message);
    }

    fn send_app_notification(&self, id: &str, title: &str, body: &str) {
        self.send_app_notification_for(id, title, body, None);
    }

    /// A notification that opens the window when clicked, on `page` if one is
    /// named. The default action is explicit: GLib's freedesktop backend
    /// (Plasma, Xfce, MATE, Cinnamon, dunst) offers a click only for one, and
    /// without it clicking a phone's notification did nothing.
    fn send_app_notification_for(&self, id: &str, title: &str, body: &str, page: Option<&str>) {
        let Some(application) = self.application() else {
            return;
        };
        let notification = gio::Notification::new(title);
        notification.set_body(Some(body));
        notification.set_icon(&gio::ThemedIcon::new("derx06.desklink.com-symbolic"));
        match page {
            Some(page) => crate::application::open_on_click(
                &notification,
                "app.show-page",
                Some(&page.to_variant()),
            ),
            None => crate::application::open_on_click(&notification, "app.show-window", None),
        }
        application.send_notification(Some(id), &notification);
    }

    /// A phone's notification as the desktop's own: opens the Notifications
    /// page; the app's icon when the phone sent one.
    fn send_phone_notification(&self, id: &str, title: &str, body: &str, icon: Option<&str>) {
        let Some(application) = self.application() else {
            return;
        };
        let notification = gio::Notification::new(title);
        notification.set_body(Some(body));
        match icon.and_then(crate::device_links::app_icons::png) {
            Some(png) => notification.set_icon(&gio::BytesIcon::new(&glib::Bytes::from(&*png))),
            None => notification.set_icon(&gio::ThemedIcon::new("derx06.desklink.com-symbolic")),
        }
        crate::application::open_on_click(
            &notification,
            "app.show-page",
            Some(&"notifications".to_variant()),
        );
        application.send_notification(Some(id), &notification);
    }

    pub(super) fn send_feature_notification(&self, id: &str, title: &str, body: &str) {
        let Some(application) = self.application() else {
            return;
        };
        let notification = gio::Notification::new(title);
        notification.set_body(Some(body));
        notification.set_priority(gio::NotificationPriority::High);
        notification.set_icon(&gio::ThemedIcon::new("derx06.desklink.com-symbolic"));
        crate::application::open_on_click(&notification, "app.show-window", None);
        application.send_notification(Some(id), &notification);
    }

    /// Tells the user how each transfer ended. The Transfers page shows the
    /// ones still moving.
    pub(super) fn follow_transfers(&self, daemon: &DaemonHandle) {
        let finished = daemon.take_finished_transfers();
        if finished.is_empty() {
            return;
        }
        let names: std::collections::HashMap<String, String> = daemon
            .devices()
            .into_iter()
            .map(|device| (device.id, device.name))
            .collect();
        for done in &finished {
            let device = names
                .get(&done.device_id)
                .map_or("your phone", String::as_str);
            self.announce_transfer(done, device);
        }
        // "Received here" has a new entry.
        self.imp().snapshots.borrow_mut().remove("files");
    }

    fn announce_transfer(&self, done: &Finished, device: &str) {
        let name = &done.name;
        let (message, notify) = match &done.outcome {
            Outcome::Received(path) => (
                format!("Received {name} from {device}. Saved in {}", saved_in(path)),
                true,
            ),
            Outcome::Sent => (format!("Sent {name} to {device}"), false),
            Outcome::Cancelled => (format!("Cancelled {name}"), false),
            Outcome::Failed(reason) => {
                let what = if done.incoming { "receive" } else { "send" };
                (format!("Could not {what} {name}: {reason}"), true)
            }
        };
        // Names and reasons come from the peer; toasts read markup by default.
        let toast = adw::Toast::new(&message);
        toast.set_use_markup(false);
        toast.set_timeout(if matches!(done.outcome, Outcome::Received(_)) {
            10
        } else {
            5
        });
        if let Outcome::Received(path) = &done.outcome {
            toast.set_button_label(Some("Show in Folder"));
            toast.set_action_name(Some("win.show-download"));
            toast.set_action_target_value(Some(&path.to_string_lossy().to_string().to_variant()));
        } else if let (Outcome::Failed(_), Some(source)) = (&done.outcome, &done.retry) {
            // A send that failed after the last block (the phone could not
            // place the file) can be sent again.
            let target = format!("{}\n{}", done.device_id, source.to_string_lossy());
            offer_retry(&target);
            toast.set_button_label(Some("Try again"));
            toast.set_action_name(Some("win.send-again"));
            toast.set_action_target_value(Some(&target.to_variant()));
        }
        self.queue_toast(&toast);
        if notify {
            self.send_app_notification(
                &format!("desklink-transfer-{}", done.id),
                "DeskLink",
                &message,
            );
        }
    }

    /// Shows the standing problem, if any, in the banner and keeps it there
    /// (called at every poll). Returns whether the banner shows it. It waits
    /// for another banner to go rather than replace it, and stays hidden once
    /// the person dismissed it.
    pub(super) fn assert_sticky_notice(&self) -> bool {
        let imp = self.imp();
        let notice = crate::platform::sticky_notice()
            .filter(|(id, _, _)| !imp.sticky_dismissed.borrow().contains(id));
        let showing = imp.sticky_id.borrow().is_some();
        match notice {
            Some((id, words, command)) if showing || !imp.error_banner.is_revealed() => {
                // An AdwBanner has one button: "Copy command" when there is
                // something to install (it also puts the banner away, as the
                // same need stays in Finish setting up), "Dismiss" otherwise.
                let title = match &command {
                    Some(_) => format!("{words} {INSTALL_HINT}"),
                    None => words,
                };
                imp.error_banner.set_title(&title);
                imp.error_banner
                    .set_button_label(Some(if command.is_some() {
                        "Copy command"
                    } else {
                        "Dismiss"
                    }));
                imp.sticky_command.replace(command);
                imp.error_banner.set_revealed(true);
                imp.sticky_id.replace(Some(id));
                true
            }
            Some(_) => false,
            None => {
                if showing {
                    imp.sticky_id.replace(None);
                    imp.error_banner.set_revealed(false);
                }
                false
            }
        }
    }

    /// The banner button on a standing problem: copies its install command
    /// when it has one, and dismisses it for this run.
    /// `false` when the banner shows an error instead.
    pub(super) fn dismiss_sticky_notice(&self) -> bool {
        let imp = self.imp();
        let Some(id) = imp.sticky_id.replace(None) else {
            return false;
        };
        if let Some(command) = imp.sticky_command.take() {
            self.clipboard().set_text(&command);
            self.show_feature_toast("Command copied");
        }
        imp.sticky_dismissed.borrow_mut().insert(id);
        imp.error_banner.set_revealed(false);
        true
    }

    /// Puts a daemon error in words a person can act on: a banner for a
    /// lasting state, a toast (once per half minute) for a passing one. The
    /// raw text stays one click away for bug reports.
    pub(super) fn report_error(&self, raw: &str, connected: bool) {
        let imp = self.imp();
        let (message, lasting) = friendly_error(raw);
        imp.last_raw_error.replace(raw.to_string());
        if connected && is_offline_message(&message) {
            debug_log!("[UI] Offline report while a device is connected: {raw}");
            return;
        }
        if lasting {
            // An error takes the banner over; a standing problem returns to
            // it once the error's time is up.
            imp.sticky_id.replace(None);
            imp.banner_since.set(Some(std::time::Instant::now()));
            imp.error_banner.set_title(&message);
            imp.error_banner.set_button_label(Some("Details"));
            imp.error_banner.set_revealed(true);
            return;
        }
        let repeated = imp
            .last_error_toast
            .borrow()
            .as_ref()
            .is_some_and(|(shown, at)| {
                *shown == message && at.elapsed() < std::time::Duration::from_secs(30)
            });
        if repeated {
            return;
        }
        imp.last_error_toast
            .replace(Some((message.clone(), std::time::Instant::now())));
        let toast = adw::Toast::new(&message);
        toast.set_use_markup(false);
        toast.set_timeout(10);
        toast.set_button_label(Some("Details"));
        toast.set_action_name(Some("win.error-details"));
        toast.set_action_target_value(Some(&raw.to_variant()));
        self.queue_toast(&toast);
        self.send_app_notification("desklink-error", "DeskLink", &message);
    }

    pub(super) fn show_error_details(&self, raw: &str) {
        if self.visible_dialog().is_some() {
            return;
        }
        let dialog = adw::AlertDialog::new(Some("Error details"), Some(&friendly_error(raw).0));
        dialog.set_extra_child(Some(
            &gtk::Label::builder()
                .label(raw)
                .selectable(true)
                .wrap(true)
                .wrap_mode(gtk::pango::WrapMode::WordChar)
                .xalign(0.0)
                .css_classes(["monospace", "dim-label"])
                .build(),
        ));
        dialog.add_responses(&[("copy", "_Copy"), ("close", "_Close")]);
        dialog.set_default_response(Some("close"));
        dialog.set_close_response("close");
        let (window, raw) = (self.downgrade(), raw.to_string());
        dialog.connect_response(Some("copy"), move |_, _| {
            if let Some(window) = window.upgrade() {
                gtk::prelude::WidgetExt::clipboard(&window).set_text(&raw);
                window.show_feature_toast("Copied");
            }
        });
        dialog.present(Some(self));
    }

    /// A Wi-Fi QR code from the phone (F3): its text is already copied;
    /// Join adds the network through NetworkManager.
    pub(super) fn offer_wifi(&self, network: crate::device_links::webrtc::capture::Wifi) {
        let toast = adw::Toast::new(&format!("Wi-Fi network “{}” scanned", network.ssid));
        toast.set_use_markup(false);
        toast.set_timeout(15);
        toast.set_button_label(Some("Join"));
        let window = self.downgrade();
        toast.connect_button_clicked(move |_| {
            let (network, window) = (network.clone(), window.clone());
            let (sender, receiver) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let _ = sender.send(crate::device_links::webrtc::capture::join_wifi(&network));
            });
            glib::timeout_add_local(std::time::Duration::from_millis(200), move || {
                let result = match try_result(&receiver) {
                    Poll::Waiting => return glib::ControlFlow::Continue,
                    Poll::Done(result) => result,
                    // The worker died without answering: there is nothing to wait for.
                    Poll::Gone => Err("NetworkManager did not answer".to_string()),
                };
                if let (Err(error), Some(window)) = (result, window.upgrade()) {
                    window.show_feature_toast(&format!("Could not join{}", because(&error)));
                }
                glib::ControlFlow::Break
            });
        });
        self.queue_toast(&toast);
    }

    /// A phone wants to open `url`, which is not a web page: asked first, with
    /// the kind named, Cancel as the default, and one question at a time.
    pub(super) fn ask_to_open_link(&self, phone: &str, url: &str) {
        thread_local! {
            static OPEN: std::cell::RefCell<Option<glib::WeakRef<adw::AlertDialog>>> =
                const { std::cell::RefCell::new(None) };
        }
        let already = OPEN.with(|slot| {
            slot.borrow()
                .as_ref()
                .and_then(glib::WeakRef::upgrade)
                .is_some_and(|dialog| dialog.is_mapped())
        });
        if already {
            return;
        }
        let kind = link_kind_words(url);
        let shown: String = url.chars().take(200).collect();
        let dialog =
            adw::AlertDialog::new(Some(&format!("Open {kind} from {phone}?")), Some(&shown));
        dialog.add_response("cancel", "_Cancel");
        dialog.add_response("open", "_Open");
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        let url = url.to_string();
        dialog.connect_response(Some("open"), move |_, _| {
            if let Err(error) = crate::device_links::daemon::open_link_now(&url) {
                eprintln!("[UI] {error}");
            }
        });
        OPEN.with(|slot| *slot.borrow_mut() = Some(dialog.downgrade()));
        dialog.present(Some(self));
    }

    /// Saves the note being typed, if any: before the app quits.
    pub(crate) fn flush_unsaved(&self) {
        if let Some(page) = self.imp().notes.borrow().as_ref() {
            page.flush();
        }
    }

    /// Adds a toast, but never a backlog: at most [`TOASTS_AT_ONCE`] wait
    /// here (the oldest is dismissed for a new one), and the same words
    /// twice are one toast. A group chat of thirty messages, or Ring pressed
    /// five times, used to queue minutes of stale toasts.
    pub(super) fn queue_toast(&self, toast: &adw::Toast) {
        thread_local! {
            static LIVE: std::cell::RefCell<Vec<glib::WeakRef<adw::Toast>>> = const { std::cell::RefCell::new(Vec::new()) };
        }
        LIVE.with(|live| {
            let mut live = live.borrow_mut();
            live.retain(|toast| toast.upgrade().is_some());
            let titles: Vec<String> = live
                .iter()
                .filter_map(|toast| toast.upgrade())
                .map(|toast| {
                    toast
                        .title()
                        .map(|title| title.to_string())
                        .unwrap_or_default()
                })
                .collect();
            match toast_plan(
                &titles,
                &toast
                    .title()
                    .map(|title| title.to_string())
                    .unwrap_or_default(),
            ) {
                ToastPlan::Skip => return,
                ToastPlan::Dismiss(count) => {
                    for old in live.drain(..count) {
                        if let Some(old) = old.upgrade() {
                            old.dismiss();
                        }
                    }
                }
                ToastPlan::Add => {}
            }
            live.push(toast.downgrade());
            self.imp().toast_overlay.add_toast(toast.clone());
        });
    }

    pub(crate) fn show_feature_toast(&self, message: &str) {
        let toast = adw::Toast::new(message);
        toast.set_use_markup(false);
        toast.set_timeout(3);
        self.queue_toast(&toast);
    }
}

/// A toast's words: at most 160 characters (a toast is one or two lines).
fn banner_line(words: &str) -> String {
    match words.char_indices().nth(160) {
        Some((at, _)) => format!("{}…", &words[..at]),
        None => words.to_string(),
    }
}
