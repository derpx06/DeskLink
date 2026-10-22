use super::discovery::broadcast_identity;
use super::pairing_commands::PairingContext;
use super::state::{mark_error, push_error};
use super::DaemonWorker;
use crate::device_links::packet::{
    NetworkPacket, PACKET_TYPE_FINDMYPHONE_REQUEST, PACKET_TYPE_LOCK_REQUEST,
    PACKET_TYPE_MOUSEPAD_REQUEST, PACKET_TYPE_MPRIS_REQUEST, PACKET_TYPE_NOTIFICATION_ACTION,
    PACKET_TYPE_NOTIFICATION_REPLY, PACKET_TYPE_NOTIFICATION_REQUEST, PACKET_TYPE_PING,
    PACKET_TYPE_SHARE_REQUEST, PACKET_TYPE_TELEPHONY_REQUEST,
};
use crate::device_links::transport::{should_surface_send_error, FeatureTransport};
use crate::device_links::webrtc::terminal_bridge;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub enum DaemonCommand {
    Discover,
    RequestPair(String),
    /// Device id and the verification key the user was shown when accepting.
    AcceptPair(String, Option<String>),
    RejectPair(String),
    Unpair(String),
    /// Show a fresh QR pairing code (see `pairing_qr`); the UI reads it back
    /// with `pairing_qr::current_uri`.
    StartQrPairing,
    CancelQrPairing,
    SendPing(String),
    SendMousepadRequest(String, serde_json::Map<String, serde_json::Value>),
    StartPhoneRemoteControl(String),
    StopPhoneRemoteControl(String),
    /// Ask again for control of a phone whose screen is being viewed.
    RequestPhoneControl(String),
    SendFile(String, std::path::PathBuf),
    /// Stops a running file transfer: the device and the transfer's id.
    CancelTransfer(String, String),
    SendShareText(String, String),
    /// Sends this computer's clipboard text to the phone now.
    SendClipboard(String),
    /// Show the phone as its own top-bar item too.
    SetTopBarPill(bool),
    /// A call request (T2): the phone ("" = the connected one that can
    /// call), the action, and the number to dial.
    Telephony(String, String, Option<String>),
    /// Insert from the phone (F3): photo / scan / text / qr into the
    /// clipboard or a file, or "cancel" the open one.
    Capture(String, String, String),
    /// Preferences → Sound: phones may hear this computer's microphone.
    SetDeskMicToPhone(bool),
    /// Sound (F1): `desk-sound` (this computer's sound to the phone),
    /// `phone-sound` or `phone-mic` (the phone's, here), on or off.
    Sound(String, String, bool),
    /// "Type on phone": keys for the phone's focused text field.
    SendFocusedFieldKeys(String, serde_json::Map<String, serde_json::Value>),
    /// Send this computer's notifications to the phones.
    SetForwardNotifications(bool),
    /// Allow (or stop) one app's notifications going to the phones.
    SetForwardApp(String, bool),
    SendLockRequest(String, bool),
    /// Allow or stop a paired phone opening terminals on this computer.
    SetTerminalAccess(String, bool),
    /// Keep this computer awake while a phone is paired.
    SetStayAwake(bool),
    /// Share the clipboard with paired phones.
    SetClipboardSync(bool),
    /// Show a phone's notifications here (device id, shown).
    SetShowNotifications(String, bool),
    /// The user answered "Not now" to the top-bar extension offer.
    SetShellExtensionPromptDismissed(bool),
    /// Relay on/off and its address ("" = the built-in one).
    SetRelay {
        enabled: bool,
        url: String,
    },
    /// Allow TURN when no direct path exists.
    SetUseTurn(bool),
    /// Start DeskLink in the background at login.
    SetStartAtLogin(bool),
    /// Tell this phone when a long terminal command finishes (device id).
    SetCommandAlerts(String, bool),
    /// Let this phone, on this network, unlock this computer when approved.
    SetUnlockNearby(String, bool),
    /// Glide (F4): the pointer glides onto this phone past this edge, or
    /// off (`None`) for it.
    SetGlide(String, Option<crate::device_links::glide::Edge>),
    /// Name this phone's callers from the contacts it shares (F5 K4).
    SetContactCallerNames(String, bool),
    /// The commands a phone may run here (replaces the list).
    /// Lock this computer and offer the newest terminal to the phones.
    LockAndHandOff,
    /// The network changed: reconnect the relay now.
    NetworkChanged,
    /// Set, change (`Some`) or remove (`None`) the session password.
    SetSessionPassword(Option<String>),
    /// Stop phones viewing/controlling this screen and detach their
    /// terminals: one device, or every device with `None`.
    EndPhoneSessions(Option<String>),
    SendFindPhone(String),
    SendMprisAction(String, String, String),
    RequestMprisStatus(String, Option<String>),
    SendMprisSetVolume(String, String, i64),
    SendMprisSeek(String, String, i64),
    /// Device, player, position in milliseconds.
    SendMprisSetPosition(String, String, i64),
    RequestNotifications(String),
    DismissNotification(String, String),
    ReplyNotification(String, String, String),
    TriggerNotificationAction(String, String, String),
    Stop,
}

impl DaemonWorker {
    pub(super) fn handle_command(&self, command: DaemonCommand) -> bool {
        match command {
            DaemonCommand::Discover => {
                if let Err(error) = broadcast_identity(&self.config, self.tcp_port) {
                    push_error(&self.errors, error);
                }
            }
            DaemonCommand::RequestPair(device_id) => self.send_pair_request(&device_id),
            DaemonCommand::StartQrPairing => {
                let started = self
                    .config
                    .lock()
                    .map_err(|_| "Config lock poisoned".to_string())
                    .and_then(|config| {
                        let der = config.certificate().to_der().map_err(|e| e.to_string())?;
                        Ok((config.local_device_info().id, der))
                    })
                    .and_then(|(id, der)| {
                        let host = lan_address().map(|ip| format!("{ip}:{}", self.tcp_port));
                        let addresses =
                            super::network::lan_ipv4s(&super::network::local_interfaces());
                        crate::device_links::pairing_qr::start(
                            &id,
                            &der,
                            host,
                            &addresses,
                            self.tcp_port,
                        )
                    });
                if let Err(error) = started {
                    push_error(
                        &self.errors,
                        format!("Could not create a pairing code: {error}"),
                    );
                }
            }
            DaemonCommand::CancelQrPairing => crate::device_links::pairing_qr::cancel(),
            DaemonCommand::AcceptPair(device_id, shown_key) => {
                debug_log!("[Daemon] Running accept_pair for {}", device_id);
                self.accept_pair(&device_id, shown_key.as_deref());
            }
            DaemonCommand::RejectPair(device_id) => {
                eprintln!("[Daemon] Running reject_pair for {}", device_id);
                self.reject_pair(&device_id);
            }
            DaemonCommand::Unpair(device_id) => self.unpair(&device_id),
            DaemonCommand::SendPing(id) => self.send_ping(&id),
            DaemonCommand::SendMousepadRequest(id, payload) => {
                self.send_mousepad_request(&id, payload)
            }
            DaemonCommand::StartPhoneRemoteControl(id) => self.start_phone_remote_control(&id),
            DaemonCommand::StopPhoneRemoteControl(id) => self.stop_phone_remote_control(&id),
            DaemonCommand::RequestPhoneControl(id) => {
                self.with_feature_transport(&id, |transport| transport.request_phone_control());
            }
            DaemonCommand::SendFile(id, path) => self.send_file(&id, path),
            DaemonCommand::CancelTransfer(id, transfer_id) => {
                self.cancel_transfer(&id, &transfer_id)
            }
            DaemonCommand::SendShareText(id, text) => self.send_share_text(&id, &text),
            DaemonCommand::SendClipboard(id) => self.send_clipboard(&id),
            DaemonCommand::SetTopBarPill(on) => {
                self.update_settings(|settings| settings.top_bar_pill = on)
            }
            DaemonCommand::Telephony(id, action, number) => {
                self.send_telephony(&id, &action, number.as_deref())
            }
            DaemonCommand::Capture(id, kind, target) => {
                let sent =
                    self.with_feature_transport(&id, |transport| transport.capture(&kind, &target));
                if !sent && kind == "cancel" {
                    crate::device_links::webrtc::capture::end(&id);
                }
            }
            DaemonCommand::SetDeskMicToPhone(on) => {
                self.update_settings(|settings| settings.desk_mic_to_phone = on)
            }
            DaemonCommand::Sound(id, what, on) => {
                let sent = self.with_feature_transport(&id, |transport| transport.sound(&what, on));
                // Off is local too: nothing plays on once the link is gone.
                if !sent && !on {
                    crate::device_links::webrtc::audio_session::end_all(&id);
                }
            }
            DaemonCommand::SendFocusedFieldKeys(id, payload) => {
                self.send_focused_field_keys(&id, payload)
            }
            DaemonCommand::SetForwardNotifications(on) => {
                self.update_settings(|settings| settings.forward_notifications = on);
                crate::device_links::desk_notifications::send_snapshot(None);
            }
            DaemonCommand::SetForwardApp(app, on) => {
                self.update_settings(|settings| {
                    if on {
                        settings.forward_apps.insert(app);
                    } else {
                        settings.forward_apps.remove(&app);
                    }
                });
                crate::device_links::desk_notifications::send_snapshot(None);
            }
            DaemonCommand::SendLockRequest(id, lock) => self.send_lock_request(&id, lock),
            DaemonCommand::SetTerminalAccess(id, allowed) => self.set_terminal_access(&id, allowed),
            DaemonCommand::EndPhoneSessions(device_id) => self.end_phone_sessions(device_id),
            DaemonCommand::SetSessionPassword(password) => self.set_session_password(password),
            DaemonCommand::SetRelay { enabled, url } => {
                self.update_settings(|settings| {
                    settings.relay_enabled = enabled;
                    settings.relay_url = url;
                });
                crate::device_links::relay_link::client::reset();
            }
            DaemonCommand::SetUseTurn(on) => {
                self.update_settings(|settings| settings.use_turn = on)
            }
            DaemonCommand::SetStartAtLogin(on) => self.set_start_at_login(on),
            DaemonCommand::SetCommandAlerts(id, on) => self.update_settings(|settings| {
                if on {
                    settings.muted_command_alerts.remove(&id);
                } else {
                    settings.muted_command_alerts.insert(id);
                }
            }),
            DaemonCommand::SetUnlockNearby(id, on) => self.update_settings(|settings| {
                if on {
                    settings.unlock_devices.insert(id);
                } else {
                    settings.unlock_devices.remove(&id);
                }
            }),
            DaemonCommand::SetGlide(id, edge) => self.update_settings(|settings| match edge {
                Some(edge) => {
                    settings.glide =
                        Some(crate::device_links::glide::GlideSetting { device: id, edge })
                }
                None if settings
                    .glide
                    .as_ref()
                    .is_some_and(|glide| glide.device == id) =>
                {
                    settings.glide = None
                }
                None => {}
            }),
            DaemonCommand::SetContactCallerNames(id, on) => self.update_settings(|settings| {
                if on {
                    settings.contact_caller_names.insert(id);
                } else {
                    settings.contact_caller_names.remove(&id);
                }
            }),
            DaemonCommand::LockAndHandOff => {
                if let Err(error) = crate::device_links::desk_features::lock_and_hand_off() {
                    push_error(&self.errors, format!("Could not hand off: {error}"));
                }
            }
            DaemonCommand::NetworkChanged => self.network_changed(),
            DaemonCommand::SetStayAwake(on) => {
                self.update_settings(|settings| settings.stay_awake = on)
            }
            DaemonCommand::SetClipboardSync(on) => {
                self.update_settings(|settings| settings.clipboard_sync = on)
            }
            DaemonCommand::SetShowNotifications(id, shown) => self.update_settings(|settings| {
                if shown {
                    settings.hidden_notifications.remove(&id);
                } else {
                    settings.hidden_notifications.insert(id);
                }
            }),
            DaemonCommand::SetShellExtensionPromptDismissed(on) => {
                self.update_settings(|settings| settings.shell_extension_prompt_dismissed = on)
            }
            DaemonCommand::SendFindPhone(id) => self.send_find_phone(&id),
            DaemonCommand::SendMprisAction(id, player, action) => {
                self.send_mpris_action(&id, &player, &action)
            }
            DaemonCommand::RequestMprisStatus(id, player) => self.request_mpris_status(&id, player),
            DaemonCommand::SendMprisSetVolume(id, player, volume) => {
                self.send_mpris_set_volume(&id, &player, volume)
            }
            DaemonCommand::SendMprisSetPosition(id, player, position_ms) => {
                self.with_feature_transport(&id, |transport| {
                    let mut packet = NetworkPacket::new(PACKET_TYPE_MPRIS_REQUEST);
                    packet.set("player", player.as_str());
                    packet.set("SetPosition", position_ms.max(0));
                    transport.send(&packet)
                });
            }
            DaemonCommand::SendMprisSeek(id, player, offset) => {
                self.send_mpris_seek(&id, &player, offset)
            }
            DaemonCommand::RequestNotifications(id) => self.request_notifications(&id),
            DaemonCommand::DismissNotification(id, notification_id) => {
                self.dismiss_notification(&id, &notification_id)
            }
            DaemonCommand::ReplyNotification(id, reply_id, message) => {
                self.reply_notification(&id, &reply_id, &message)
            }
            DaemonCommand::TriggerNotificationAction(id, key, action) => {
                self.trigger_notification_action(&id, &key, &action)
            }
            DaemonCommand::Stop => return true,
        }
        false
    }
    /// The pairing commands live on [`PairingContext`] so they can run without
    /// a `DaemonWorker`, and therefore without sockets.
    fn pairing(&self) -> PairingContext {
        PairingContext {
            sessions: Arc::clone(&self.sessions),
            devices: Arc::clone(&self.devices),
            config: Arc::clone(&self.config),
            errors: Arc::clone(&self.errors),
        }
    }

    fn send_pair_request(&self, device_id: &str) {
        self.pairing().send_pair_request(device_id);
    }

    fn accept_pair(&self, device_id: &str, shown_key: Option<&str>) {
        self.pairing().accept_pair(device_id, shown_key);
    }

    fn reject_pair(&self, device_id: &str) {
        self.pairing().reject_pair(device_id);
    }

    fn unpair(&self, device_id: &str) {
        self.pairing().unpair(device_id);
    }

    fn send_ping(&self, device_id: &str) {
        self.with_feature_transport(device_id, |transport| {
            let mut packet = NetworkPacket::new(PACKET_TYPE_PING);
            packet.set("message", "Ping from DeskLink");
            transport.send(&packet)
        });
    }

    fn send_mousepad_request(
        &self,
        device_id: &str,
        payload: serde_json::Map<String, serde_json::Value>,
    ) {
        self.with_feature_transport(device_id, |transport| {
            let packet = NetworkPacket::with_body(PACKET_TYPE_MOUSEPAD_REQUEST, payload);
            transport.send_remote_input(packet)
        });
    }

    /// Keys typed into the phone's focused field: only over a ready link,
    /// never queued (late keys would land in whatever has focus by then).
    fn send_focused_field_keys(
        &self,
        device_id: &str,
        mut payload: serde_json::Map<String, serde_json::Value>,
    ) {
        payload.insert("target".into(), crate::type_on_phone::TARGET.into());
        self.with_feature_transport(device_id, |transport| {
            if !transport.is_ready() {
                return Err("Session link is closed".to_string());
            }
            transport.send(&NetworkPacket::with_body(
                PACKET_TYPE_MOUSEPAD_REQUEST,
                payload,
            ))
        });
    }

    fn start_phone_remote_control(&self, device_id: &str) {
        self.with_feature_transport(device_id, |transport| {
            transport.start_phone_remote_control()
        });
    }

    fn stop_phone_remote_control(&self, device_id: &str) {
        self.with_feature_transport(device_id, |transport| transport.stop_phone_remote_control());
    }

    fn send_share_text(&self, device_id: &str, text: &str) {
        self.with_feature_transport(device_id, |transport| {
            let packet = build_share_text_packet(text)?;
            transport.send(&packet)
        });
    }

    fn send_lock_request(&self, device_id: &str, lock: bool) {
        self.with_feature_transport(device_id, |transport| {
            let mut packet = NetworkPacket::new(PACKET_TYPE_LOCK_REQUEST);
            packet.set("setLocked", lock);
            transport.send(&packet)
        });
    }

    /// Persists the "Allow terminal access" choice and, when it is being taken
    /// away, cuts the phone off from every terminal immediately.
    ///
    /// The setting is written first: once it reads *off*, no new request can get
    /// past the gate, and the revocation then only has to clear what is already
    /// attached. A failure is shown on the device rather than swallowed, and
    /// also refreshes the card so its switch cannot keep saying "on".
    fn set_terminal_access(&self, device_id: &str, allowed: bool) {
        debug_log!("[DeskLink] Terminal access device={device_id:?} allowed={allowed}");
        let saved = self
            .config
            .lock()
            .map_err(|_| "Config lock poisoned".to_string())
            .and_then(|mut config| config.set_terminal_access(device_id, allowed));
        if !allowed {
            terminal_bridge::revoke_device(device_id);
        }
        if let Err(error) = saved {
            mark_error(
                &self.devices,
                device_id,
                format!("Could not change terminal access: {error}"),
            );
        }
    }

    fn end_phone_sessions(&self, device_id: Option<String>) {
        let ids: Vec<String> = match device_id {
            Some(id) => vec![id],
            None => self
                .devices
                .lock()
                .map(|devices| devices.keys().cloned().collect())
                .unwrap_or_default(),
        };
        for id in ids {
            crate::device_links::webrtc::coordinator::end_phone_sessions(&id);
        }
    }

    fn set_session_password(&self, password: Option<String>) {
        self.set_session_password_only(password);
    }

    fn set_session_password_only(&self, password: Option<String>) {
        use crate::device_links::session_lock;
        let hash = match password.as_deref().map(session_lock::hash_password) {
            Some(Err(error)) => {
                push_error(&self.errors, format!("Could not set the password: {error}"));
                return;
            }
            Some(Ok(hash)) => Some(hash),
            None => None,
        };
        let locking = hash.is_some();
        self.update_settings(|settings| settings.session_password = hash);
        // Phones that proved the old password must prove the new one — and
        // phones already in must not stay in on the strength of the old rule.
        if let Ok(mut lock) = session_lock::shared().lock() {
            lock.relock_all();
        }
        if locking {
            self.end_phone_sessions(None);
        }
    }

    pub(super) fn set_start_at_login(&self, on: bool) {
        match crate::autostart::set(on) {
            Ok(()) => self.update_settings(|settings| settings.start_at_login = Some(on)),
            Err(error) => push_error(
                &self.errors,
                format!("Could not change starting at login: {error}"),
            ),
        }
    }

    fn update_settings(&self, change: impl FnOnce(&mut crate::device_links::config::AppSettings)) {
        let saved = self
            .config
            .lock()
            .map_err(|_| "Config lock poisoned".to_string())
            .and_then(|mut config| config.update_settings(change));
        if let Err(error) = saved {
            push_error(&self.errors, format!("Could not save the setting: {error}"));
        }
        // Every phone shows the desk's settings; tell them.
        crate::device_links::desk_features::push_settings();
    }

    fn send_telephony(&self, device_id: &str, action: &str, number: Option<&str>) {
        let chosen = self.devices.lock().ok().and_then(|devices| {
            let phones = || devices.values().filter(|device| device.trusted);
            if device_id.is_empty() {
                phones()
                    .filter(|device| device.feature_transport_ready)
                    .max_by_key(|device| {
                        device
                            .incoming_capabilities
                            .iter()
                            .any(|capability| capability == PACKET_TYPE_TELEPHONY_REQUEST)
                    })
                    .cloned()
            } else {
                phones().find(|device| device.id == device_id).cloned()
            }
        });
        let Some(device) = chosen else {
            push_error(
                &self.errors,
                "No phone is connected to make the call.".to_string(),
            );
            return;
        };
        match crate::device_links::calls::request(device.call_status.as_ref(), action, number) {
            Ok(packet) => {
                self.with_feature_transport(&device.id, |transport| transport.send(&packet));
            }
            Err(error) => push_error(&self.errors, error),
        }
    }

    fn send_find_phone(&self, device_id: &str) {
        self.with_feature_transport(device_id, |transport| {
            let packet = NetworkPacket::new(PACKET_TYPE_FINDMYPHONE_REQUEST);
            transport.send(&packet)
        });
    }

    fn send_mpris_action(&self, device_id: &str, player: &str, action: &str) {
        let sent = self.with_feature_transport(device_id, |transport| {
            let packet = build_mpris_action_packet(player, action);
            transport.send(&packet)
        });
        // Shown at once, but only for a send that went: the phone's next
        // state report confirms or corrects it.
        if let Ok(mut devices) = self.devices.lock() {
            if let Some(device) = devices
                .get_mut(device_id)
                .filter(|device| sent && device.feature_transport_ready)
            {
                if let Some(media) = device.media_status.as_mut() {
                    media.is_playing = optimistic_playing(media.is_playing, action);
                }
            }
        }
    }

    fn request_mpris_status(&self, device_id: &str, player: Option<String>) {
        self.with_feature_transport(device_id, |transport| {
            let packet = build_mpris_status_request_packet(player.as_deref());
            transport.send(&packet)
        });
    }

    fn send_mpris_set_volume(&self, device_id: &str, player: &str, volume: i64) {
        self.with_feature_transport(device_id, |transport| {
            let packet = build_mpris_set_volume_packet(player, volume)?;
            transport.send(&packet)
        });
    }

    fn send_mpris_seek(&self, device_id: &str, player: &str, offset: i64) {
        self.with_feature_transport(device_id, |transport| {
            let packet = build_mpris_seek_packet(player, offset)?;
            transport.send(&packet)
        });
    }

    fn request_notifications(&self, device_id: &str) {
        self.with_feature_transport(device_id, |transport| {
            let packet = build_notification_request_packet();
            transport.send(&packet)
        });
    }

    /// The network changed (Wi-Fi switched, a cable plugged): recover in
    /// about two seconds instead of waiting for silence. Announce again, drop
    /// LAN links on addresses this computer no longer has, and rebuild peers
    /// whose addresses all went; the redialer and the phones' answers to the
    /// beacons bring the links back.
    fn network_changed(&self) {
        crate::device_links::relay_link::client::reset();
        super::discovery::beacon_again();
        let present: Vec<std::net::IpAddr> = super::network::local_interfaces()
            .into_iter()
            .map(|(_, ip)| ip)
            .collect();
        for binding in self.sessions.current_bindings() {
            if binding
                .link
                .local_ip()
                .is_some_and(|ip| !present.contains(&ip))
            {
                super::handshake::handle_disconnect(
                    &binding,
                    &self.devices,
                    &self.sessions,
                    "This computer's network changed".to_string(),
                );
            }
        }
        crate::device_links::webrtc::coordinator::rebuild_peers_off_the_network(
            &present,
            &self.sessions,
            &self.config,
            &self.devices,
            &self.events,
        );
    }

    fn send_clipboard(&self, device_id: &str) {
        let text = arboard::Clipboard::new()
            .and_then(|mut clipboard| clipboard.get_text())
            .unwrap_or_default();
        if text.is_empty() {
            push_error(
                &self.errors,
                "The clipboard has no text to send".to_string(),
            );
            return;
        }
        self.with_feature_transport(device_id, |transport| {
            let mut packet = NetworkPacket::new(crate::device_links::packet::PACKET_TYPE_CLIPBOARD);
            packet.set("content", text);
            transport.send(&packet)
        });
    }

    fn dismiss_notification(&self, device_id: &str, notification_id: &str) {
        self.with_feature_transport(device_id, |transport| {
            let packet = build_notification_dismiss_packet(notification_id)?;
            transport.send(&packet)
        });
    }

    fn reply_notification(&self, device_id: &str, reply_id: &str, message: &str) {
        self.with_feature_transport(device_id, |transport| {
            let packet = build_notification_reply_packet(reply_id, message)?;
            transport.send(&packet)
        });
    }

    fn trigger_notification_action(&self, device_id: &str, key: &str, action: &str) {
        self.with_feature_transport(device_id, |transport| {
            let packet = build_notification_action_packet(key, action)?;
            transport.send(&packet)
        });
    }
    /// Builds a generation-checked transport snapshot under the session lock,
    /// then runs the caller after the lock has been released.  Feature sends
    /// therefore cannot block link replacement, shutdown, or ICE recovery.
    /// Runs a user's command against `device_id`'s feature transport.
    /// Whether it was sent (or queued for the moment the link is ready).
    ///
    /// A command the user asked for is answered: "not connected" reaches the
    /// UI instead of the log. Only a session being replaced underneath the
    /// send stays quiet — that is recovery, and the device page shows it.
    fn with_feature_transport(
        &self,
        device_id: &str,
        f: impl FnOnce(&FeatureTransport) -> Result<(), String>,
    ) -> bool {
        let result = FeatureTransport::for_device(&self.sessions, device_id)
            .and_then(|transport| f(&transport));
        let Err(error) = result else {
            return true;
        };
        match command_failure(&error) {
            CommandFailure::Recovering => {
                eprintln!("[DeskLink] Feature send deferred during WebRTC recovery: {error}");
            }
            failure @ (CommandFailure::NotConnected | CommandFailure::Unsupported) => {
                let name = self
                    .devices
                    .lock()
                    .ok()
                    .and_then(|devices| devices.get(device_id).map(|device| device.name.clone()))
                    .unwrap_or_else(|| "The phone".to_string());
                push_error(
                    &self.errors,
                    if failure == CommandFailure::Unsupported {
                        format!("{name} doesn't support this yet (update DeskLink on the phone).")
                    } else {
                        format!("{name} is not connected. Try again once it is.")
                    },
                );
            }
            CommandFailure::Failed => {
                mark_error(&self.devices, device_id, error.clone());
                push_error(&self.errors, error);
            }
        }
        false
    }
}

/// How a failed user command is reported.
#[derive(Debug, PartialEq, Eq)]
enum CommandFailure {
    /// The session is being replaced (or the WebRTC handover is in
    /// progress): recovery, not a failure. Logged.
    Recovering,
    /// No link to the phone: the user is told so.
    NotConnected,
    /// The phone's DeskLink does not take this feature (an older build, or
    /// the feature is off between the two): the user is told to update.
    Unsupported,
    /// Anything else, shown as it is.
    Failed,
}

fn command_failure(error: &str) -> CommandFailure {
    if error.contains("replaced by a newer session generation")
        || error.contains("Stale DeskLink session")
    {
        CommandFailure::Recovering
    } else if error.contains("is not enabled in the DeskLink")
        || error.contains("DeskLink has no outgoing handler for")
        || error.contains("does not accept")
    {
        CommandFailure::Unsupported
    } else if !should_surface_send_error(error) {
        CommandFailure::NotConnected
    } else {
        CommandFailure::Failed
    }
}

fn build_share_text_packet(text: &str) -> Result<NetworkPacket, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("Shared text is empty".to_string());
    }

    let mut packet = NetworkPacket::new(PACKET_TYPE_SHARE_REQUEST);
    if text.starts_with("http://") || text.starts_with("https://") {
        packet.set("url", text);
    } else {
        packet.set("text", text);
    }
    Ok(packet)
}

fn build_mpris_action_packet(player: &str, action: &str) -> NetworkPacket {
    let mut packet = NetworkPacket::new(PACKET_TYPE_MPRIS_REQUEST);
    let player = player.trim();
    if !player.is_empty() {
        packet.set("player", player);
    }
    packet.set("action", action);
    packet
}

fn build_mpris_status_request_packet(player: Option<&str>) -> NetworkPacket {
    let mut packet = NetworkPacket::new(PACKET_TYPE_MPRIS_REQUEST);
    if let Some(player) = player.map(str::trim).filter(|player| !player.is_empty()) {
        packet.set("player", player);
        packet.set("requestNowPlaying", true);
        packet.set("requestVolume", true);
    } else {
        packet.set("requestPlayerList", true);
    }
    packet
}

fn build_mpris_set_volume_packet(player: &str, volume: i64) -> Result<NetworkPacket, String> {
    let player = player.trim();
    if player.is_empty() {
        return Err("Media player is required before setting volume".to_string());
    }
    let mut packet = NetworkPacket::new(PACKET_TYPE_MPRIS_REQUEST);
    packet.set("player", player);
    packet.set("setVolume", volume.clamp(0, 150));
    Ok(packet)
}

fn build_mpris_seek_packet(player: &str, offset: i64) -> Result<NetworkPacket, String> {
    let player = player.trim();
    if player.is_empty() {
        return Err("Media player is required before seeking".to_string());
    }
    let mut packet = NetworkPacket::new(PACKET_TYPE_MPRIS_REQUEST);
    packet.set("player", player);
    packet.set("Seek", offset);
    Ok(packet)
}

fn build_notification_request_packet() -> NetworkPacket {
    let mut packet = NetworkPacket::new(PACKET_TYPE_NOTIFICATION_REQUEST);
    packet.set("request", true);
    packet
}

fn build_notification_dismiss_packet(notification_id: &str) -> Result<NetworkPacket, String> {
    let notification_id = notification_id.trim();
    if notification_id.is_empty() {
        return Err("Notification id is required".to_string());
    }
    let mut packet = NetworkPacket::new(PACKET_TYPE_NOTIFICATION_REQUEST);
    packet.set("cancel", notification_id);
    Ok(packet)
}

fn build_notification_reply_packet(reply_id: &str, message: &str) -> Result<NetworkPacket, String> {
    let reply_id = reply_id.trim();
    let message = message.trim();
    if reply_id.is_empty() {
        return Err("Notification reply id is required".to_string());
    }
    if message.is_empty() {
        return Err("Notification reply message is empty".to_string());
    }
    let mut packet = NetworkPacket::new(PACKET_TYPE_NOTIFICATION_REPLY);
    packet.set("requestReplyId", reply_id);
    packet.set("message", message);
    Ok(packet)
}

fn build_notification_action_packet(key: &str, action: &str) -> Result<NetworkPacket, String> {
    let key = key.trim();
    let action = action.trim();
    if key.is_empty() || action.is_empty() {
        return Err("Notification key and action are required".to_string());
    }
    let mut packet = NetworkPacket::new(PACKET_TYPE_NOTIFICATION_ACTION);
    packet.set("key", key);
    packet.set("action", action);
    Ok(packet)
}
/// The address other LAN devices reach this desktop on: the source address of
/// the default route. `connect` on UDP only selects a route; nothing is sent.
fn lan_address() -> Option<std::net::IpAddr> {
    let socket = std::net::UdpSocket::bind(("0.0.0.0", 0)).ok()?;
    socket.connect(("192.0.2.1", 9)).ok()?;
    let ip = socket.local_addr().ok()?.ip();
    (!ip.is_unspecified() && !ip.is_loopback()).then_some(ip)
}

/// What a play/pause action will make of `playing`, shown before the phone
/// confirms it.
fn optimistic_playing(playing: bool, action: &str) -> bool {
    match action {
        "PlayPause" => !playing,
        "Play" => true,
        "Pause" | "Stop" => false,
        _ => playing,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_command_is_reported_and_recovery_is_not() {
        assert_eq!(
            command_failure("Device is not connected"),
            CommandFailure::NotConnected
        );
        assert_eq!(
            command_failure("Session link is closed"),
            CommandFailure::NotConnected
        );
        assert_eq!(
            command_failure("Binding was replaced by a newer session generation"),
            CommandFailure::Recovering
        );
        assert_eq!(
            command_failure("Could not encode the packet"),
            CommandFailure::Failed
        );
        assert_eq!(
            command_failure(
                "desklink.mpris.request is not enabled in the DeskLink WebRTC feature profile"
            ),
            CommandFailure::Unsupported
        );
        assert_eq!(
            command_failure("The paired device does not accept desklink.x"),
            CommandFailure::Unsupported
        );
    }

    #[test]
    fn play_pause_flips_at_once_and_other_actions_do_not() {
        assert!(!optimistic_playing(true, "PlayPause"));
        assert!(optimistic_playing(false, "PlayPause"));
        assert!(optimistic_playing(false, "Play"));
        assert!(!optimistic_playing(true, "Pause"));
        assert!(optimistic_playing(true, "Next"));
    }
    use crate::device_links::packet::{
        PACKET_TYPE_MPRIS_REQUEST, PACKET_TYPE_NOTIFICATION_ACTION, PACKET_TYPE_NOTIFICATION_REPLY,
        PACKET_TYPE_NOTIFICATION_REQUEST, PACKET_TYPE_SHARE_REQUEST,
    };

    #[test]
    fn share_text_packet_uses_text_body_for_plain_text() {
        let packet = build_share_text_packet("hello desklink").unwrap();

        assert_eq!(packet.packet_type, PACKET_TYPE_SHARE_REQUEST);
        assert_eq!(packet.get_str("text"), Some("hello desklink"));
        assert_eq!(packet.get_str("url"), None);
    }

    #[test]
    fn share_text_packet_uses_url_body_for_http_urls() {
        let packet = build_share_text_packet("https://kde.org").unwrap();

        assert_eq!(packet.packet_type, PACKET_TYPE_SHARE_REQUEST);
        assert_eq!(packet.get_str("url"), Some("https://kde.org"));
        assert_eq!(packet.get_str("text"), None);
    }

    #[test]
    fn share_text_packet_rejects_blank_text() {
        assert_eq!(
            build_share_text_packet("   ").unwrap_err(),
            "Shared text is empty"
        );
    }

    #[test]
    fn mpris_action_packet_omits_empty_player() {
        let packet = build_mpris_action_packet("", "PlayPause");

        assert_eq!(packet.packet_type, PACKET_TYPE_MPRIS_REQUEST);
        assert_eq!(packet.get_str("action"), Some("PlayPause"));
        assert_eq!(packet.get_str("player"), None);
    }

    #[test]
    fn mpris_action_packet_includes_player_when_provided() {
        let packet = build_mpris_action_packet("org.mpris.MediaPlayer2.vlc", "Next");

        assert_eq!(packet.packet_type, PACKET_TYPE_MPRIS_REQUEST);
        assert_eq!(packet.get_str("player"), Some("org.mpris.MediaPlayer2.vlc"));
        assert_eq!(packet.get_str("action"), Some("Next"));
    }

    #[test]
    fn mpris_status_request_asks_for_player_list_without_player() {
        let packet = build_mpris_status_request_packet(None);

        assert_eq!(packet.packet_type, PACKET_TYPE_MPRIS_REQUEST);
        assert_eq!(packet.get_bool("requestPlayerList"), Some(true));
    }

    #[test]
    fn mpris_status_request_asks_for_now_playing_and_volume_with_player() {
        let packet = build_mpris_status_request_packet(Some("vlc"));

        assert_eq!(packet.packet_type, PACKET_TYPE_MPRIS_REQUEST);
        assert_eq!(packet.get_str("player"), Some("vlc"));
        assert_eq!(packet.get_bool("requestNowPlaying"), Some(true));
        assert_eq!(packet.get_bool("requestVolume"), Some(true));
    }

    #[test]
    fn notification_reply_packet_contains_reply_id_and_message() {
        let packet = build_notification_reply_packet("reply-1", "hello").unwrap();

        assert_eq!(packet.packet_type, PACKET_TYPE_NOTIFICATION_REPLY);
        assert_eq!(packet.get_str("requestReplyId"), Some("reply-1"));
        assert_eq!(packet.get_str("message"), Some("hello"));
    }

    #[test]
    fn notification_action_packet_contains_key_and_action() {
        let packet = build_notification_action_packet("key-1", "Open").unwrap();

        assert_eq!(packet.packet_type, PACKET_TYPE_NOTIFICATION_ACTION);
        assert_eq!(packet.get_str("key"), Some("key-1"));
        assert_eq!(packet.get_str("action"), Some("Open"));
    }

    #[test]
    fn notification_request_packet_requests_current_notifications() {
        let packet = build_notification_request_packet();

        assert_eq!(packet.packet_type, PACKET_TYPE_NOTIFICATION_REQUEST);
        assert_eq!(packet.get_bool("request"), Some(true));
    }
}
