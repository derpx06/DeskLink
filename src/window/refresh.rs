//! Keeping the window current (`impl DesklinkWindow`): connecting it to the
//! daemon, refreshing the device list and the page, layout and tile columns,
//! the first frame and trimming memory while hidden.
//!
//! Sibling edges: `imp`, `navigation`/`notices` (the same window type), `words`,
//! `blocks`, `debug_verbs` (the e2e action, debug builds).

use super::*;

impl DesklinkWindow {
    pub fn set_daemon(&self, daemon: DaemonHandle) {
        // Debug builds only: lets `tools/device_e2e.sh` drive desktop-initiated
        // features over the window's exported D-Bus action group, e.g.
        // `gdbus call --session --dest derx06.desklink.com
        //   --object-path /derx06/desklink/com/window/1
        //   --method org.gtk.Actions.Activate e2e "[<'ping ID'>]" {}`
        #[cfg(debug_assertions)]
        {
            let e2e_daemon = daemon.clone();
            self.add_action_entries([gio::ActionEntry::builder("e2e")
                .parameter_type(Some(glib::VariantTy::STRING))
                .activate(move |win: &Self, _, parameter| {
                    let Some(line) = parameter.and_then(|value| value.get::<String>()) else {
                        return;
                    };
                    let mut parts = line.splitn(3, ' ');
                    let (verb, id) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
                    let (id, arg) = (id.to_string(), parts.next().unwrap_or("").to_string());
                    if debug_ui_verb(win, verb, &id, &arg) {
                        return;
                    }
                    // "sound ID desk-sound|phone-sound|phone-mic on|off" (F1).
                    if verb == "sound" {
                        let (what, state) = arg.split_once(' ').unwrap_or((&arg, "on"));
                        e2e_daemon.send(DaemonCommand::Sound(id, what.into(), state == "on"));
                        return;
                    }
                    // "glide ID left|right|top|bottom|off" (F4; an `e2e-*`
                    // id glides with no phone, its packets logged).
                    if verb == "glide" {
                        let edge = crate::device_links::glide::Edge::ALL
                            .into_iter()
                            .find(|edge| format!("{edge:?}").eq_ignore_ascii_case(&arg));
                        let id = if id == "selected" {
                            win.selected_id().unwrap_or_default()
                        } else {
                            id
                        };
                        e2e_daemon.send(DaemonCommand::SetGlide(id, edge));
                        return;
                    }
                    if verb == "desk-terminal" {
                        let result = crate::device_links::pty::desk::new_desk_session();
                        debug_log!("[E2E] desk-terminal: {result:?}");
                        return;
                    }
                    // "extend-selftest WxH": F2's second display without a phone.
                    if verb == "extend-selftest" {
                        let size = id
                            .split_once('x')
                            .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))
                            .unwrap_or((2400, 1080));
                        let _ = std::thread::Builder::new()
                            .name("desklink-extend-selftest".into())
                            .spawn(move || {
                                let result =
                                    crate::device_links::webrtc::virtual_display::selftest(size);
                                debug_log!("[E2E] extend-selftest: {result:?}");
                            });
                        return;
                    }
                    if verb == "desktop-selftest" {
                        let _ = std::thread::Builder::new()
                            .name("desklink-share-selftest".into())
                            .spawn(move || {
                                let result =
                                    crate::device_links::webrtc::screen::desktop_share_selftest(
                                        &id,
                                    );
                                debug_log!("[E2E] desktop-selftest: {result:?}");
                            });
                        return;
                    }
                    // "desk-input ID {mousepad body}": as if the phone sent it.
                    if verb == "desk-input" {
                        let result =
                            crate::device_links::webrtc::debug_inject_desktop_input(&id, &arg);
                        debug_log!("[E2E] desk-input {arg}: {result:?}");
                        return;
                    }
                    if verb == "remote-window" {
                        crate::remote_control::show_remote_control_dialog(
                            e2e_daemon.clone(),
                            id,
                            win,
                        );
                        return;
                    }
                    let command = match verb {
                        "request-pair" => DaemonCommand::RequestPair(id),
                        // Accepts with the key the desktop is showing, as a
                        // user comparing codes would.
                        "accept-pair" => {
                            let key = e2e_daemon
                                .devices()
                                .into_iter()
                                .find(|device| device.id == id)
                                .and_then(|device| device.verification_key);
                            match key {
                                Some(key) => DaemonCommand::AcceptPair(id, Some(key)),
                                None => return,
                            }
                        }
                        "ping" => DaemonCommand::SendPing(id),
                        "find-phone" => DaemonCommand::SendFindPhone(id),
                        "lock-phone" => DaemonCommand::SendLockRequest(id, true),
                        "send-file" => DaemonCommand::SendFile(id, arg.into()),
                        "share-text" => DaemonCommand::SendShareText(id, arg),
                        "notifications" => DaemonCommand::RequestNotifications(id),
                        "terminal-access" => DaemonCommand::SetTerminalAccess(id, arg == "on"),
                        "view-phone" => DaemonCommand::StartPhoneRemoteControl(id),
                        // "type TEXT" / "key N": the phone window's keyboard path.
                        "type" => {
                            let mut payload = serde_json::Map::new();
                            payload.insert("key".into(), arg.clone().into());
                            DaemonCommand::SendMousepadRequest(id, payload)
                        }
                        "key" => {
                            let mut payload = serde_json::Map::new();
                            payload.insert(
                                "specialKey".into(),
                                arg.parse::<i64>().unwrap_or(0).into(),
                            );
                            DaemonCommand::SendMousepadRequest(id, payload)
                        }
                        "stop-phone" => DaemonCommand::StopPhoneRemoteControl(id),
                        // "tap X Y" on the phone, through the real lease path.
                        "tap" => {
                            let mut parts = arg.split(' ').filter_map(|v| v.parse::<i64>().ok());
                            let (Some(x), Some(y)) = (parts.next(), parts.next()) else {
                                return;
                            };
                            let mut payload = serde_json::Map::new();
                            payload.insert("singleclick".into(), true.into());
                            payload.insert("x".into(), x.into());
                            payload.insert("y".into(), y.into());
                            DaemonCommand::SendMousepadRequest(id, payload)
                        }
                        _ => {
                            debug_log!("[E2E] unknown command: {line}");
                            return;
                        }
                    };
                    debug_log!("[E2E] {verb}");
                    e2e_daemon.send(command);
                })
                .build()]);
        }
        self.imp().daemon.replace(Some(daemon));
        // The tickers run every second while this window shows and slowly
        // when it does not: showing it brings them back at once.
        self.connect_map(|window| {
            crate::device_links::daemon::set_ui_visible(true);
            // The first time, the page is drawn just after the window's first
            // frame and faded in, so drawing it never holds the window back.
            if window.imp().shown_once.replace(true) {
                window.refresh_soon();
            } else {
                window.draw_after_first_frame();
            }
            // Every ticker was waiting for a window nobody could see.
            crate::ticker::nudge_all();
        });
        self.connect_unmap(|window| {
            crate::device_links::daemon::set_ui_visible(false);
            // A window that stays hidden gives its memory back: the pictures
            // go, the page that held them is redrawn on the next look, and
            // the allocator returns what it can.
            let window = window.downgrade();
            glib::timeout_add_seconds_local_once(HIDDEN_TRIM_SECONDS, move || {
                let Some(window) = window.upgrade().filter(|window| !window.is_mapped()) else {
                    return;
                };
                window.trim_memory();
            });
        });
        // A window nobody is looking at (another desktop, behind others)
        // stops its pulsing dots, so it costs no frames.
        self.connect_is_active_notify(mark_focus);
        mark_focus(self);
        self.refresh_devices();
        // Events (a call ringing, a ping) refresh at once, not at the tick.
        crate::device_links::daemon::state::set_ui_wake(refresh_open_windows);

        let weak = self.downgrade();
        crate::ticker::every_tick(move || {
            crate::guard("window refresh", glib::ControlFlow::Continue, || {
                if let Some(window) = weak.upgrade() {
                    window.refresh_devices();
                    glib::ControlFlow::Continue
                } else {
                    glib::ControlFlow::Break
                }
            })
        });
    }

    /// The devices the window knows: paired ones and those mid-pairing.
    pub(super) fn known_devices(&self, daemon: &DaemonHandle) -> Vec<DeviceView> {
        #[allow(unused_mut)]
        let mut devices = daemon.devices();
        #[cfg(debug_assertions)]
        devices.extend(self.imp().fake_devices.borrow().iter().cloned());
        // A phone that did not accept our request stays, with why.
        let (refused, newly) = pairing::watch(&devices);
        for name in newly {
            self.show_feature_toast(&format!("{name} didn't accept the request"));
        }
        let refused_ids: Vec<String> = refused.iter().map(|gone| gone.id.clone()).collect();
        devices.retain(|device| !refused_ids.contains(&device.id));
        devices.extend(refused);
        devices
            .into_iter()
            .filter(|device| is_listed(device, refused_ids.contains(&device.id)))
            .collect()
    }

    pub(super) fn refresh_devices(&self) {
        crate::device_links::daemon::set_ui_visible(self.is_visible());
        let imp = self.imp();
        // Not while a click is under way: a page or list redrawn between
        // press and release takes the button away and the click is lost.
        // The release refreshes (`hold_redraws_while_pressed`).
        if imp.refreshing.get() || imp.pointer_held.get() {
            return;
        }
        let Some(daemon) = imp.daemon.borrow().clone() else {
            return;
        };
        imp.refreshing.set(true);
        // RAII rather than a set at the end: this returns early from several
        // places, and any missed reset would wedge the window permanently.
        let _guard = RefreshGuard(&imp.refreshing);

        // The transfer cards' own timer runs only while something moves.
        if let Some(transfers) = imp.transfers.borrow().as_ref() {
            transfers.kick();
        }

        let errors = daemon.drain_errors();
        for event in daemon.drain_events() {
            match event {
                DaemonEvent::PingReceived {
                    device_id,
                    device_name,
                    message,
                } => {
                    self.send_feature_notification(
                        &format!("desklink-ping-{device_id}-{}", glib::monotonic_time()),
                        &format!("Nudge from {device_name}"),
                        message
                            .as_deref()
                            .unwrap_or("Your phone sent a nudge through DeskLink."),
                    );
                    self.show_feature_toast(&format!("Nudge from {device_name}"));
                }
                DaemonEvent::FindPhoneReceived {
                    device_id,
                    device_name,
                } => {
                    self.send_feature_notification(
                        &format!("desklink-find-phone-{device_id}-{}", glib::monotonic_time()),
                        &format!("Find Phone request from {device_name}"),
                        "Your phone asked DeskLink to ring this computer.",
                    );
                    self.show_feature_toast(&format!("Find Phone request from {device_name}"));
                }
                DaemonEvent::PairedByQr {
                    device_id,
                    device_name,
                } => {
                    // The scan is done: leave "Pair another phone" for the phone
                    // it just paired. The page renders later in this refresh.
                    let mut was_pairing = false;
                    if let Some(dialog) = self.visible_dialog() {
                        if matches!(
                            dialog.title().as_str(),
                            pairing::PAIR_TITLE
                                | pairing::PAIR_FIRST_TITLE
                                | pairing::PAIR_CODE_TITLE
                        ) {
                            dialog.close();
                            was_pairing = true;
                        }
                    }
                    // Both the code path and the QR path report; once is enough.
                    if !was_pairing
                        && imp.selected_device_id.borrow().as_deref() == Some(device_id.as_str())
                    {
                        continue;
                    }
                    imp.selected_device_id.replace(Some(device_id.clone()));
                    imp.chosen_device_id.replace(Some(device_id.clone()));
                    sidebar::save_last_device(&device_id);
                    imp.page.replace("home".to_string());
                    imp.split_view.set_show_content(true);
                    self.show_feature_toast(&format!("Paired with {device_name}"));
                }
                DaemonEvent::PhoneScreenOffered { device_id } => {
                    crate::remote_control::show_remote_control_dialog(
                        daemon.clone(),
                        device_id,
                        self,
                    );
                }
                DaemonEvent::MissedCall {
                    device_id,
                    device_name,
                    caller,
                    number,
                } => calls::missed(self, &device_id, &device_name, caller, number),
                DaemonEvent::CallRefused { message } | DaemonEvent::LockRefused { message } => {
                    self.show_feature_toast(&message)
                }
                DaemonEvent::PairingReplaced { name } => {
                    let text = format!(
                        "The earlier pairing of {name} was replaced. If that was a different phone, pair it again."
                    );
                    self.send_feature_notification(
                        &format!("desklink-replaced-{}", glib::monotonic_time()),
                        &format!("Replaced the old pairing of {name}"),
                        &text,
                    );
                    self.show_feature_toast(&text);
                }
                DaemonEvent::SoundNotice { message, .. }
                | DaemonEvent::CaptureNotice { message, .. }
                | DaemonEvent::GlideNotice { message, .. } => self.show_feature_toast(&message),
                DaemonEvent::LinkRequest { phone, url } => self.ask_to_open_link(&phone, &url),
                DaemonEvent::LinksHeld { device_id } => {
                    if let Some(app) = self
                        .application()
                        .and_downcast::<crate::DesklinkApplication>()
                    {
                        app.show_held_links(&device_id);
                    }
                }
                DaemonEvent::WifiFromPhone { network, .. } => self.offer_wifi(network),
                // Handled by this refresh itself (the call card and notification).
                DaemonEvent::CallChanged { .. } => {}
            }
        }

        self.follow_transfers(&daemon);
        self.show_gallery_arrivals(&daemon.devices());

        let devices = self.known_devices(&daemon);
        // A device connected over the network proves the network is up: an
        // "offline" report then is stale (e.g. the relay failing), not news.
        let connected = devices
            .iter()
            .any(|device| matches!(device.status, DeviceStatus::Paired));
        if let Some(error) = errors.last() {
            self.report_error(error, connected);
        }
        {
            // When each active session started, for "since 14:02".
            let mut since = imp.session_since.borrow_mut();
            since.retain(|id, _| {
                devices
                    .iter()
                    .any(|device| &device.id == id && session_summary(device).is_some())
            });
            for device in devices.iter().filter(|d| session_summary(d).is_some()) {
                since
                    .entry(device.id.clone())
                    .or_insert_with(|| clock_time(glib::DateTime::now_local().ok()));
            }
        }
        if connected && is_offline_message(&imp.error_banner.title()) {
            imp.error_banner.set_revealed(false);
            imp.banner_since.set(None);
        }
        self.mirror_device_side_effects(&daemon, &devices, errors.is_empty());
        calls::notify(self, &devices);

        let selected = imp.selected_device_id.borrow().clone();
        let show = device_to_show(
            imp.chosen_device_id.borrow().as_deref(),
            selected.as_deref(),
            &devices,
        );
        if show != selected {
            imp.selected_device_id.replace(show);
            imp.snapshots.borrow_mut().clear();
        }

        // The first-run tour: decided once, when the paired phones are known.
        if !imp.onboarding_decided.get() {
            use crate::device_links::daemon::handle::PairedPhones;
            match daemon.paired_phones() {
                PairedPhones::Pending => return,
                // The config could not be read: "no phone" means nothing.
                PairedPhones::Unknown => imp.onboarding_decided.set(true),
                PairedPhones::Known => {
                    imp.onboarding_decided.set(true);
                    // Read again now that they are known: `devices` may be a
                    // snapshot from before the worker listed them.
                    let any_paired = self
                        .known_devices(&daemon)
                        .iter()
                        .chain(devices.iter())
                        .any(|device| device.trusted);
                    if onboarding::show_on_start(
                        crate::device_links::config::app_settings().onboarding_seen,
                        any_paired,
                    ) {
                        self.start_onboarding(0);
                    }
                }
            }
        }
        let tour = imp.onboarding.borrow().clone();
        if let Some(tour) = tour {
            let paired = devices.iter().any(|device| device.trusted);
            if tour.for_pairing && !devices.is_empty() {
                // A phone paired (the tour is done), or asked to pair: the
                // pairing flow as ever, and a request that comes to nothing
                // leaves the tour unseen.
                self.end_onboarding(false, paired);
            } else {
                if let Some(stop) = imp.welcome_qr.take() {
                    pairing::stop_welcome(&daemon, &stop);
                }
                if imp.main_stack.visible_child_name().as_deref() != Some("onboarding") {
                    imp.main_stack.set_visible_child_name("onboarding");
                }
                return;
            }
        }

        if imp.main_stack.visible_child_name().as_deref() == Some("solo") {
            // Stays until Back, even if a phone pairs meanwhile.
            if let Some(stop) = imp.welcome_qr.take() {
                pairing::stop_welcome(&daemon, &stop);
            }
            if let Some(solo) = imp.solo.borrow().as_ref() {
                solo.update();
            }
            return;
        }
        if devices.is_empty() {
            imp.notified_pair_requests.borrow_mut().clear();
            imp.notified_remote_notifications.borrow_mut().clear();
            if imp.main_stack.visible_child_name().as_deref() != Some("welcome") {
                imp.main_stack.set_visible_child_name("welcome");
                imp.snapshots.borrow_mut().clear();
            }
            if imp.welcome_qr.borrow().is_none() {
                let shown = pairing::welcome_page(self, &daemon);
                imp.welcome_qr.replace(Some(shown));
                if imp.onboarding_to_pairing.get() {
                    imp.welcome_body.prepend(&self.onboarding_last_step());
                }
            }
            return;
        }
        if let Some(stop) = imp.welcome_qr.take() {
            pairing::stop_welcome(&daemon, &stop);
        }
        if imp.main_stack.visible_child_name().as_deref() != Some("devices") {
            imp.main_stack.set_visible_child_name("devices");
            let page = imp.page.borrow().clone();
            imp.page_stack.set_visible_child_name(&page);
            self.update_title();
        }
        let selected_id = imp.selected_device_id.borrow().clone();
        let Some(device) = devices
            .iter()
            .find(|device| Some(&device.id) == selected_id.as_ref())
            .cloned()
        else {
            return;
        };
        // Nothing to draw for a window nobody can see: pages are rebuilt (and
        // were leaking menus) for no one. It redraws when it is shown again.
        if !should_draw_pages(self.is_mapped()) {
            return;
        }
        if let Some(sidebar) = imp.sidebar.borrow().as_ref() {
            sidebar.update(self, &devices, &device);
        }
        self.render_page(&daemon, &device);
        calls::update_bar(self, &daemon, &devices);
    }

    /// Draws the page on show, if what it shows has changed.
    fn render_page(&self, daemon: &DaemonHandle, device: &DeviceView) {
        let imp = self.imp();
        let mut page = imp.page.borrow().clone();
        // A device still pairing has only its pairing view.
        if !device.trusted && page != "home" {
            page = "home".to_string();
            imp.page.replace(page.clone());
            imp.page_stack.set_visible_child_name(&page);
            self.update_title();
        }
        match page.as_str() {
            "terminals" => {
                if let Some(terminals) = imp.terminals.borrow().as_ref() {
                    terminals.update(self, device);
                }
            }
            "notifications" => {
                if let Some(notifications) = imp.notifications.borrow().as_ref() {
                    notifications.update(self, daemon, device);
                }
            }
            "files" => {
                bucket::update(self, device);
                if let Some(transfers) = imp.transfers.borrow().as_ref() {
                    transfers.update(self, daemon, device);
                }
            }
            "home" => overview::update(self, daemon, device),
            "notes" => {
                if let Some(notes) = imp.notes.borrow().as_ref() {
                    notes.update(device);
                }
            }
            "contacts" => {
                if let Some(contacts) = imp.contacts.borrow().as_ref() {
                    contacts.update(self, daemon, device);
                }
            }
            "settings" => prefs::update(self, daemon, device),
            _ => {}
        }
    }

    /// Whether page `name` must be redrawn for `key`, recording it.
    pub(super) fn page_changed(&self, name: &str, key: String) -> bool {
        let previous = self
            .imp()
            .snapshots
            .borrow_mut()
            .insert(name.to_string(), key.clone());
        previous.as_ref() != Some(&key)
    }

    /// Replaces page `name`'s content, keeping its scroll position and, when
    /// it is the same device, where keyboard focus was.
    pub(super) fn replace_body(&self, name: &str, content: &impl IsA<gtk::Widget>) {
        let Some((scroll, body)) = self.imp().bodies.borrow().get(name).cloned() else {
            return;
        };
        let body_widget = body.clone().upcast::<gtk::Widget>();
        let focus_path =
            gtk::prelude::RootExt::focus(self).and_then(|focus| widget_path(&body_widget, &focus));
        let adjustment = scroll.vadjustment();
        let offset = adjustment.value();
        while let Some(child) = body.first_child() {
            body.remove(&child);
        }
        body.append(content);
        self.apply_layout();
        adjustment.set_value(offset);
        let body = body.downgrade();
        glib::idle_add_local_once(move || {
            adjustment.set_value(offset);
            let Some(body) = body.upgrade() else {
                return;
            };
            if let Some(widget) = focus_path.and_then(|path| widget_at(body.upcast_ref(), &path)) {
                widget.grab_focus();
            }
        });
    }

    /// Sets how many tiles the Home row holds from the page's width (the
    /// page's margins off), now and once the layout has settled.
    pub(super) fn apply_tile_columns(&self) {
        let imp = self.imp();
        for widget in named_widgets(imp.page_stack.upcast_ref(), "tile-row") {
            // The width the row really has (one Home column of two on a big
            // window), and how wide a tile is with its words on one line.
            let width = widget
                .parent()
                .map(|column| column.width())
                .unwrap_or_default();
            if width <= 0 {
                continue;
            }
            let tile = widget
                .first_child()
                .map(|child| child.measure(gtk::Orientation::Horizontal, -1).1)
                .unwrap_or_default();
            let columns = overview::tile_columns(width, tile);
            if let Some(flow) = widget.downcast_ref::<gtk::FlowBox>() {
                if flow.max_children_per_line() != columns {
                    flow.set_max_children_per_line(columns);
                }
            }
        }
    }

    /// Lays named boxes and grids out for the window's width: the overview's
    /// two columns and the hero stack when phone-narrow, the terminal grid
    /// goes one to a line.
    pub(super) fn apply_layout(&self) {
        let narrow = self.imp().narrow.get();
        let compact = narrow || self.imp().compact.get();
        let wide = !compact && self.imp().wide.get();
        // Tighter page edges, for `style.css`.
        if narrow {
            self.add_css_class("narrow");
        } else {
            self.remove_css_class("narrow");
        }
        let mut stack = vec![self.imp().main_stack.get().upcast::<gtk::Widget>()];
        while let Some(widget) = stack.pop() {
            match widget.widget_name().as_str() {
                "responsive" => {
                    if let Some(row) = widget.downcast_ref::<gtk::Box>() {
                        row.set_orientation(if compact {
                            gtk::Orientation::Vertical
                        } else {
                            gtk::Orientation::Horizontal
                        });
                    }
                }
                // Home: two columns side by side on a big window, else one.
                "home-split" => {
                    if let Some(split) = widget.downcast_ref::<gtk::Box>() {
                        split.set_orientation(if wide {
                            gtk::Orientation::Horizontal
                        } else {
                            gtk::Orientation::Vertical
                        });
                        split.set_homogeneous(wide);
                        split.set_spacing(if wide { 24 } else { 16 });
                    }
                }
                "home-clamp" => {
                    if let Some(clamp) = widget.downcast_ref::<adw::Clamp>() {
                        clamp.set_maximum_size(if wide {
                            overview::HOME_WIDE_WIDTH
                        } else {
                            overview::HOME_WIDTH
                        });
                    }
                }
                "terminal-grid" => {
                    if let Some(grid) = widget.downcast_ref::<gtk::FlowBox>() {
                        grid.set_max_children_per_line(match (narrow, wide) {
                            (true, _) => 1,
                            (_, true) => 4,
                            _ => 3,
                        });
                    }
                }
                // Files and the Bucket: wider, more cards a line on a big
                // window; each card keeps its size.
                "files-clamp" => {
                    if let Some(clamp) = widget.downcast_ref::<adw::Clamp>() {
                        clamp.set_maximum_size(if wide { FILES_WIDE_WIDTH } else { FILES_WIDTH });
                    }
                }
                "card-grid" | "bucket-grid" => {
                    if let Some(grid) = widget.downcast_ref::<gtk::FlowBox>() {
                        let normal = if widget.widget_name() == "card-grid" {
                            5
                        } else {
                            6
                        };
                        grid.set_max_children_per_line(if wide { 8 } else { normal });
                    }
                }
                // Right-aligned beside the hero's name; under it when the
                // hero stacks.
                "hero-actions" => widget.set_halign(if compact {
                    gtk::Align::Fill
                } else {
                    gtk::Align::End
                }),
                // Notes: the list or the note, one at a time, when narrow;
                // on a big window a wider list (the note keeps its measure).
                "notes-split" => {
                    if let Some(split) = widget.downcast_ref::<adw::NavigationSplitView>() {
                        split.set_collapsed(compact);
                        split.set_max_sidebar_width(if wide { 420.0 } else { 320.0 });
                        split.set_sidebar_width_fraction(if wide { 0.32 } else { 0.25 });
                    }
                }
                // Notes and Snippets with no phone: the switcher.
                "solo-switcher" | "tabs-switcher" => widget.set_visible(!narrow),
                "solo-bar" | "tabs-bar" => {
                    if let Some(bar) = widget.downcast_ref::<adw::ViewSwitcherBar>() {
                        bar.set_reveal(narrow);
                    }
                }
                "hero-wrap" => widget.set_halign(if compact {
                    gtk::Align::Start
                } else {
                    gtk::Align::End
                }),
                _ => {}
            }
            let mut child = widget.first_child();
            while let Some(next) = child {
                child = next.next_sibling();
                stack.push(next);
            }
        }
        self.apply_tile_columns();
        // Again on the next frames, once the new layout has its widths: when
        // a breakpoint changes the columns, the width read now is the old one
        // (an idle could still run before the new allocation).
        let frames = std::cell::Cell::new(0);
        self.add_tick_callback(move |window, _| {
            window.apply_tile_columns();
            frames.set(frames.get() + 1);
            if frames.get() < 3 {
                glib::ControlFlow::Continue
            } else {
                glib::ControlFlow::Break
            }
        });
    }

    /// Runs `refresh_devices` once the current handler has returned, e.g.
    /// after a click whose own button the refresh may replace.
    /// The window has been hidden a while: drops decoded pictures and the
    /// Files page's cards (redrawn when it is shown again) and hands free
    /// memory back to the system.
    fn trim_memory(&self) {
        let imp = self.imp();
        transfers::forget_pictures();
        if let Some(transfers) = imp.transfers.borrow().as_ref() {
            transfers.release_cards();
        }
        // Redrawn on the next look.
        imp.snapshots.borrow_mut().remove("bucket");
        self.replace_body("bucket", &gtk::Box::new(gtk::Orientation::Vertical, 0));
        // SAFETY: malloc_trim has no preconditions; it only returns free memory.
        unsafe {
            libc::malloc_trim(0);
        }
    }

    /// The page, drawn once the first frame is on screen, fading in.
    fn draw_after_first_frame(&self) {
        let Some(clock) = self.frame_clock() else {
            return self.refresh_soon();
        };
        let handler = std::rc::Rc::new(std::cell::Cell::new(None));
        let once = handler.clone();
        let window = self.downgrade();
        handler.set(Some(clock.connect_after_paint(move |clock| {
            if let Some(id) = once.take() {
                clock.disconnect(id);
            }
            let Some(window) = window.upgrade() else {
                return;
            };
            let page = window.imp().page_stack.get();
            page.set_opacity(0.0);
            adw::TimedAnimation::new(
                &page,
                0.0,
                1.0,
                FIRST_FADE_MS,
                adw::PropertyAnimationTarget::new(&page, "opacity"),
            )
            .play();
            window.refresh_soon();
            glib::idle_add_local_once(|| crate::startup("first page drawn"));
        })));
    }

    pub(super) fn refresh_soon(&self) {
        let window = self.downgrade();
        // Guarded like the tick: events (`wake_ui`) refresh through here, and a
        // panic cannot unwind into GLib.
        glib::idle_add_local_once(move || {
            crate::guard("window refresh", (), || {
                if let Some(window) = window.upgrade() {
                    window.refresh_devices();
                }
            });
        });
    }
}
