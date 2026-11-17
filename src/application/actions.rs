//! The application's actions (`impl DesklinkApplication`): the GActions and
//! their accelerators, opening received files, the resume and network
//! watchers, the crash report, and the History, About, Preferences and
//! Shortcuts windows.
//!
//! Sibling edges: none (the parent's imp and helpers). Outside: `window`
//! (history, preferences), `about`, `send_to`, `device_links` (`daemon`,
//! `folders`, `logind`, `walk_away`, `webrtc`, `transfer_log`).

use super::*;

impl DesklinkApplication {
    pub(super) fn setup_gactions(&self) {
        let quit_action = gio::ActionEntry::builder("quit")
            .activate(move |app: &Self, _, _| app.quit())
            .build();
        let about_action = gio::ActionEntry::builder("about")
            .activate(move |app: &Self, _, _| app.show_about())
            .build();
        let preferences_action = gio::ActionEntry::builder("preferences")
            .activate(move |app: &Self, _, _| app.show_preferences())
            .build();
        let shortcuts_action = gio::ActionEntry::builder("shortcuts")
            .activate(move |app: &Self, _, _| app.show_shortcuts())
            .build();
        // Target: a device id, or "" for every device. Used by the session
        // notification's button and the tray menu.
        let end_sessions_action = gio::ActionEntry::builder("end-sessions")
            .parameter_type(Some(glib::VariantTy::STRING))
            .activate(move |app: &Self, _, target| {
                let id = target
                    .and_then(|target| target.get::<String>())
                    .filter(|id| !id.is_empty());
                if let Some(daemon) = app.daemon_handle() {
                    daemon.send(crate::device_links::daemon::DaemonCommand::EndPhoneSessions(id));
                }
            })
            .build();
        // Lock this computer and offer the newest terminal to the phones:
        // the tray, the top-bar menu and the overview.
        let lock_hand_off_action = gio::ActionEntry::builder("lock-hand-off")
            .activate(move |app: &Self, _, _| {
                if let Some(daemon) = app.daemon_handle() {
                    daemon.send(crate::device_links::daemon::DaemonCommand::LockAndHandOff);
                }
            })
            .build();
        // A phone's Bucket page (the sync notification, the tray, the top bar).
        let open_bucket_action = gio::ActionEntry::builder("open-bucket")
            .parameter_type(Some(glib::VariantTy::STRING))
            .activate(move |app: &Self, _, target| {
                let window = app.main_window();
                super::present_window(&window);
                if let Some(device) = target.and_then(|target| target.get::<String>()) {
                    window.show_device(&device);
                }
                window.show_page("files");
            })
            .build();
        // "Turn on" on the notification that says a phone asked for a Bucket.
        let turn_on_bucket_action = gio::ActionEntry::builder("bucket-turn-on")
            .parameter_type(Some(glib::VariantTy::STRING))
            .activate(move |app: &Self, _, target| {
                if let Some(device) = target.and_then(|target| target.get::<String>()) {
                    // Any process on the session bus can activate this: it
                    // only answers an ask the phone actually made.
                    if !bucket_ask_pending(&crate::device_links::folders::offers(), &device) {
                        return;
                    }
                    crate::device_links::folders::turn_on_bucket(&device);
                    app.withdraw_notification(&format!("bucket-ask-{device}"));
                }
            })
            .build();
        // "Cancel" on the walk-away warning. It only ever keeps the computer
        // unlocked for one absence; nothing outward.
        let walk_away_cancel_action = gio::ActionEntry::builder("walk-away-cancel")
            .parameter_type(Some(glib::VariantTy::STRING))
            .activate(move |app: &Self, _, target| {
                if let Some(device) = target.and_then(|target| target.get::<String>()) {
                    crate::device_links::walk_away::cancel(&device);
                    app.withdraw_notification(&format!("walk-away-{device}"));
                }
            })
            .build();
        // "Cancel" on the file transfer notification: every transfer with that
        // phone that is running or waiting (never the Bucket's own sync).
        let cancel_transfers_action = gio::ActionEntry::builder("cancel-transfers")
            .parameter_type(Some(glib::VariantTy::STRING))
            .activate(move |app: &Self, _, target| {
                let (Some(device), Some(daemon)) = (
                    target.and_then(|target| target.get::<String>()),
                    app.daemon_handle(),
                ) else {
                    return;
                };
                for row in crate::device_links::webrtc::transfer_board::rows() {
                    if row.device_id == device {
                        daemon.send(crate::device_links::daemon::DaemonCommand::CancelTransfer(
                            device.clone(),
                            row.id,
                        ));
                    }
                }
            })
            .build();
        // "Open" and "Show in Files" on a received file's notification. Any
        // process on the session bus can activate them, so only a file that is
        // in the received history opens.
        let open_received_action = gio::ActionEntry::builder("open-received")
            .parameter_type(Some(glib::VariantTy::STRING))
            .activate(move |app: &Self, _, target| {
                if let Some(path) = target.and_then(|target| target.get::<String>()) {
                    app.open_received(&path, false);
                }
            })
            .build();
        let show_received_action = gio::ActionEntry::builder("show-received")
            .parameter_type(Some(glib::VariantTy::STRING))
            .activate(move |app: &Self, _, target| {
                if let Some(path) = target.and_then(|target| target.get::<String>()) {
                    app.open_received(&path, true);
                }
            })
            .build();
        // The "closed to the background" notification's click: with no tray
        // to reopen from, the notification is the way back.
        let show_window_action = gio::ActionEntry::builder("show-window")
            .activate(move |app: &Self, _, _| super::present_window(&app.main_window()))
            .build();
        // A notification's click: the window, on the page it is about.
        let show_page_action = gio::ActionEntry::builder("show-page")
            .parameter_type(Some(glib::VariantTy::STRING))
            .activate(move |app: &Self, _, target| {
                let window = app.main_window();
                super::present_window(&window);
                if let Some(page) = target.and_then(|target| target.get::<String>()) {
                    window.show_page(&page);
                }
            })
            .build();
        let history_action = gio::ActionEntry::builder("history")
            .activate(move |app: &Self, _, _| app.show_history())
            .build();
        // (device, action, number): a call button in a notification. Answer,
        // decline, end and the like go straight to the phone; a *dial* never
        // does: anything on the session bus can activate this action, so
        // starting a call is asked first, in the window, like every other dial.
        let call_action = gio::ActionEntry::builder("call")
            .parameter_type(Some(glib::VariantTy::new("(sss)").expect("valid type")))
            .activate(move |app: &Self, _, target| {
                let Some((device, action, number)) =
                    target.and_then(|target| target.get::<(String, String, String)>())
                else {
                    return;
                };
                match call_action_kind(&action) {
                    CallAction::Dial => app.dial_with(&number, &device),
                    CallAction::Message => app.message_with(&number, &device),
                    CallAction::OnTheCall => {
                        if let Some(daemon) = app.daemon_handle() {
                            daemon.send(crate::device_links::daemon::DaemonCommand::Telephony(
                                device, action, None,
                            ));
                        }
                    }
                    CallAction::Refused => {}
                }
            })
            .build();
        // A number from outside (`tel:` links, `desklink --dial`): asked
        // first, in the window.
        let dial_action = gio::ActionEntry::builder("dial")
            .parameter_type(Some(glib::VariantTy::STRING))
            .activate(move |app: &Self, _, target| {
                if let Some(number) = target.and_then(|target| target.get::<String>()) {
                    app.dial(&number);
                }
            })
            .build();
        // "Open" on the held-links notification: (device, url). Any process
        // on the session bus can activate it, so only a link that phone sent
        // and that is still held opens.
        let open_held_link_action = gio::ActionEntry::builder("open-held-link")
            .parameter_type(Some(glib::VariantTy::new("(ss)").expect("valid type")))
            .activate(move |app: &Self, _, target| {
                let Some((device, url)) =
                    target.and_then(|target| target.get::<(String, String)>())
                else {
                    return;
                };
                if crate::device_links::daemon::take_held_link(&device, &url) {
                    if let Err(error) = crate::device_links::daemon::open_link_now(&url) {
                        eprintln!("[DeskLink] {error}");
                    }
                    app.show_held_links(&device);
                }
            })
            .build();
        // Allow / Not now on a phone's ask (Ask on computer): the one way
        // the asked setting is turned on, from the person's click here.
        let ask_answer_action = gio::ActionEntry::builder("ask-answer")
            .parameter_type(Some(glib::VariantTy::new("(ssb)").expect("valid type")))
            .activate(move |app: &Self, _, target| {
                if let Some((device, key, allow)) =
                    target.and_then(|target| target.get::<(String, String, bool)>())
                {
                    app.answer_ask(&device, &key, allow);
                }
            })
            .build();
        crate::send_to::add_action(self);
        crate::send_to::add_bucket_action(self);
        crate::send_to::add_direct_actions(self);
        self.add_action_entries([
            quit_action,
            about_action,
            preferences_action,
            shortcuts_action,
            end_sessions_action,
            lock_hand_off_action,
            history_action,
            show_window_action,
            show_page_action,
            open_bucket_action,
            turn_on_bucket_action,
            walk_away_cancel_action,
            cancel_transfers_action,
            open_received_action,
            show_received_action,
            call_action,
            dial_action,
            open_held_link_action,
            ask_answer_action,
        ]);
    }
}

impl DesklinkApplication {
    /// Opens a received file, or the folder it is in: only one the received
    /// history knows.
    fn open_received(&self, path: &str, show_folder: bool) {
        let path = std::path::PathBuf::from(path);
        let known = crate::device_links::transfer_log::recent()
            .iter()
            .any(|file| file.path == path);
        if !known || !path.exists() {
            return;
        }
        let launcher = gtk::FileLauncher::new(Some(&gio::File::for_path(&path)));
        let window = self.active_window();
        if show_folder {
            launcher.open_containing_folder(window.as_ref(), gio::Cancellable::NONE, |_| {});
        } else {
            launcher.launch(window.as_ref(), gio::Cancellable::NONE, |_| {});
        }
    }

    /// A resume from sleep is a sign of the network changing under the
    /// links: after the network has had two seconds to come back, the same
    /// recovery runs as for a network change.
    pub(super) fn watch_resume(&self) {
        let app = self.downgrade();
        crate::device_links::logind::watch_resume(move || {
            let app = app.clone();
            glib::timeout_add_seconds_local_once(2, move || {
                if let Some(daemon) = app.upgrade().and_then(|app| app.daemon_handle()) {
                    daemon.send(crate::device_links::daemon::DaemonCommand::NetworkChanged);
                }
            });
        });
    }

    /// Tells the daemon whenever the OS says the network changed (debounced:
    /// one switch fires several signals); it acts only on a real change.
    pub(super) fn watch_network(&self) {
        let pending: std::rc::Rc<std::cell::Cell<Option<glib::SourceId>>> = Default::default();
        let app = self.downgrade();
        gio::NetworkMonitor::default().connect_network_changed(move |_, _| {
            if let Some(source) = pending.take() {
                source.remove();
            }
            let app = app.clone();
            let pending_inner = pending.clone();
            pending.set(Some(glib::timeout_add_seconds_local_once(2, move || {
                pending_inner.set(None);
                // The daemon decides whether it touched a phone's link.
                if let Some(daemon) = app.upgrade().and_then(|app| app.daemon_handle()) {
                    daemon.send(crate::device_links::daemon::DaemonCommand::NetworkMaybeChanged);
                }
            })));
        });
    }

    /// Mentions a crash report written since the last start, once.
    pub(super) fn report_previous_crash(&self) {
        let Some(dir) = crate::crash_dir() else {
            return;
        };
        if !self.is_registered() || !take_unreported_crash(&dir) {
            return;
        }
        let notification = crate::application::desk_notification("DeskLink stopped unexpectedly");
        notification.set_body(Some(&format!(
            "A crash report was saved in {}. Attaching it to a bug report helps fix this.",
            dir.display()
        )));
        self.send_notification(Some("crash-report"), &notification);
    }

    /// Connection History, a dialog (Settings -> Advanced opens the same).
    fn show_history(&self) {
        let window = self.main_window();
        super::present_window(&window);
        if window.visible_dialog().is_some() {
            return;
        }
        crate::window::history::show_dialog(&window, &self.device_types(), None);
    }

    fn show_about(&self) {
        let Some(window) = self.window_for_dialog() else {
            return;
        };
        crate::about::present(&window, self.daemon_handle().as_ref());
    }

    /// The Settings page, or the computer-wide part as a dialog while no
    /// phone is paired.
    fn show_preferences(&self) {
        let window = self.main_window();
        super::present_window(&window);
        if window.show_page("settings") || window.visible_dialog().is_some() {
            return;
        }
        if let Some(daemon) = self.daemon_handle() {
            crate::window::prefs::show_dialog(&window, &daemon);
        }
    }

    /// The stock shortcuts dialog from `shortcuts-dialog.ui` where libadwaita
    /// has one (1.8); before that, the same shortcuts in a plain list.
    fn show_shortcuts(&self) {
        let Some(parent) = self.window_for_dialog() else {
            return;
        };
        if adw::minor_version() >= 8 {
            let builder = gtk::Builder::new();
            match builder.add_from_resource("/derx06/desklink/com/shortcuts-dialog.ui") {
                Ok(()) => {
                    if let Some(dialog) = builder.object::<adw::Dialog>("shortcuts_dialog") {
                        dialog.present(Some(&parent));
                        return;
                    }
                }
                Err(error) => eprintln!("[UI] No shortcuts dialog: {error}"),
            }
        }
        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();
        for (title, shortcut) in [
            ("Home, Files, Notes & clipboard", "Ctrl+1 to 3"),
            ("Notifications, Terminals, Contacts", "Ctrl+4 to 6"),
            ("Pair another phone", "Ctrl+N"),
            ("Send files", "Ctrl+O"),
            ("Find anything", "Ctrl+K"),
            ("Search this page", "Ctrl+F"),
            ("Connection history", "Ctrl+H"),
            ("Settings", "Ctrl+,"),
            ("Keyboard shortcuts", "Ctrl+?"),
            ("Close window", "Ctrl+W"),
            ("Quit", "Ctrl+Q"),
        ] {
            // Plain text: a row title is markup, and the "&" in the first
            // one left it blank.
            let row = adw::ActionRow::builder()
                .title(title)
                .use_markup(false)
                .build();
            row.add_suffix(
                &gtk::Label::builder()
                    .label(shortcut)
                    .valign(gtk::Align::Center)
                    .css_classes(["dim-label", "monospace"])
                    .build(),
            );
            list.append(&row);
        }
        let content = adw::Clamp::builder()
            .child(&list)
            .margin_top(12)
            .margin_bottom(24)
            .margin_start(12)
            .margin_end(12)
            .build();
        adw::Dialog::builder()
            .title("Keyboard shortcuts")
            .content_width(420)
            .child(&with_header(&content))
            .build()
            .present(Some(&parent));
    }
}
