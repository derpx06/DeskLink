use super::discovery::broadcast_identity;
use super::pairing_commands::PairingContext;
use super::state::{mark_error, push_error};
use super::DaemonWorker;
use crate::device_links::packet::{
    NetworkPacket, PACKET_TYPE_FINDMYPHONE_REQUEST, PACKET_TYPE_LOCK_REQUEST,
    PACKET_TYPE_MOUSEPAD_REQUEST, PACKET_TYPE_MPRIS_REQUEST, PACKET_TYPE_NOTIFICATION_ACTION,
    PACKET_TYPE_NOTIFICATION_REPLY, PACKET_TYPE_NOTIFICATION_REQUEST, PACKET_TYPE_PING,
    PACKET_TYPE_RUNCOMMAND_REQUEST, PACKET_TYPE_SFTP_REQUEST, PACKET_TYPE_SHARE_REQUEST,
    PACKET_TYPE_SYSTEMVOLUME_REQUEST,
};
use crate::device_links::transport::{should_surface_send_error, FeatureTransport};
use crate::device_links::webrtc::file_browser;
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
    /// A phone-file browser request that has already been registered with
    /// [`file_browser::begin_request`]: device ID, request ID, wire bytes.
    SendPhoneFileRequest(String, String, Vec<u8>),
    SendFile(String, std::path::PathBuf),
    /// Stops a running file transfer: the device and the transfer's id.
    CancelTransfer(String, String),
    SendShareText(String, String),
    SendLockRequest(String, bool),
    /// Allow or stop a paired phone opening terminals on this computer.
    SetTerminalAccess(String, bool),
    /// Keep this computer awake while a phone is paired.
    SetStayAwake(bool),
    /// Relay on/off and its address ("" = the built-in one).
    SetRelay {
        enabled: bool,
        url: String,
    },
    /// Allow TURN when no direct path exists.
    SetUseTurn(bool),
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
    SendSftpRequest(String),
    RequestNotifications(String),
    DismissNotification(String, String),
    ReplyNotification(String, String, String),
    TriggerNotificationAction(String, String, String),
    RequestSystemVolume(String),
    SetSystemVolume(String, String, Option<i64>, Option<bool>),
    RequestRemoteCommands(String),
    ExecuteRemoteCommand(String, String),
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
                        crate::device_links::pairing_qr::start(&id, &der, host)
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
                eprintln!("[Daemon] Running accept_pair for {}", device_id);
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
            DaemonCommand::SendPhoneFileRequest(id, request_id, payload) => {
                self.send_phone_file_request(&id, &request_id, &payload)
            }
            DaemonCommand::SendFile(id, path) => self.send_file(&id, path),
            DaemonCommand::CancelTransfer(id, transfer_id) => {
                self.cancel_transfer(&id, &transfer_id)
            }
            DaemonCommand::SendShareText(id, text) => self.send_share_text(&id, &text),
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
            DaemonCommand::NetworkChanged => crate::device_links::relay_link::client::reset(),
            DaemonCommand::SetStayAwake(on) => {
                self.update_settings(|settings| settings.stay_awake = on)
            }
            DaemonCommand::SendFindPhone(id) => self.send_find_phone(&id),
            DaemonCommand::SendMprisAction(id, player, action) => {
                self.send_mpris_action(&id, &player, &action)
            }
            DaemonCommand::RequestMprisStatus(id, player) => self.request_mpris_status(&id, player),
            DaemonCommand::SendMprisSetVolume(id, player, volume) => {
                self.send_mpris_set_volume(&id, &player, volume)
            }
            DaemonCommand::SendMprisSeek(id, player, offset) => {
                self.send_mpris_seek(&id, &player, offset)
            }
            DaemonCommand::SendSftpRequest(id) => self.send_sftp_request(&id),
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
            DaemonCommand::RequestSystemVolume(id) => self.request_system_volume(&id),
            DaemonCommand::SetSystemVolume(id, name, volume, muted) => {
                self.set_system_volume(&id, &name, volume, muted)
            }
            DaemonCommand::RequestRemoteCommands(id) => self.request_remote_commands(&id),
            DaemonCommand::ExecuteRemoteCommand(id, key) => self.execute_remote_command(&id, &key),
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

    fn start_phone_remote_control(&self, device_id: &str) {
        self.with_feature_transport(device_id, |transport| {
            transport.start_phone_remote_control()
        });
    }

    fn stop_phone_remote_control(&self, device_id: &str) {
        self.with_feature_transport(device_id, |transport| transport.stop_phone_remote_control());
    }

    /// A failure here belongs in the file dialog that asked, not in the global
    /// error banner, so it is routed back as that request's reply.
    fn send_phone_file_request(&self, device_id: &str, request_id: &str, payload: &[u8]) {
        let result = FeatureTransport::for_device(&self.sessions, device_id)
            .and_then(|transport| transport.send_phone_file_request(payload));
        if let Err(error) = result {
            file_browser::fail_request(device_id, request_id, &error);
        }
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
        eprintln!("[DeskLink] Terminal access device={device_id:?} allowed={allowed}");
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

    fn update_settings(&self, change: impl FnOnce(&mut crate::device_links::config::AppSettings)) {
        let saved = self
            .config
            .lock()
            .map_err(|_| "Config lock poisoned".to_string())
            .and_then(|mut config| config.update_settings(change));
        if let Err(error) = saved {
            push_error(&self.errors, format!("Could not save the setting: {error}"));
        }
    }

    fn send_find_phone(&self, device_id: &str) {
        self.with_feature_transport(device_id, |transport| {
            let packet = NetworkPacket::new(PACKET_TYPE_FINDMYPHONE_REQUEST);
            transport.send(&packet)
        });
    }

    fn send_mpris_action(&self, device_id: &str, player: &str, action: &str) {
        self.with_feature_transport(device_id, |transport| {
            let packet = build_mpris_action_packet(player, action);
            transport.send(&packet)
        });
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

    fn send_sftp_request(&self, device_id: &str) {
        self.with_feature_transport(device_id, |transport| {
            let packet = build_sftp_request_packet();
            transport.send(&packet)
        });
    }

    fn request_notifications(&self, device_id: &str) {
        self.with_feature_transport(device_id, |transport| {
            let packet = build_notification_request_packet();
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

    fn request_system_volume(&self, device_id: &str) {
        self.with_feature_transport(device_id, |transport| {
            let packet = build_system_volume_request_packet();
            transport.send(&packet)
        });
    }

    fn set_system_volume(
        &self,
        device_id: &str,
        name: &str,
        volume: Option<i64>,
        muted: Option<bool>,
    ) {
        self.with_feature_transport(device_id, |transport| {
            let packet = build_system_volume_set_packet(name, volume, muted)?;
            transport.send(&packet)
        });
    }

    fn request_remote_commands(&self, device_id: &str) {
        self.with_feature_transport(device_id, |transport| {
            let packet = build_remote_command_list_request_packet();
            transport.send(&packet)
        });
    }

    fn execute_remote_command(&self, device_id: &str, key: &str) {
        self.with_feature_transport(device_id, |transport| {
            let key = key.trim();
            let advertised = self
                .devices
                .lock()
                .ok()
                .and_then(|devices| devices.get(device_id).cloned())
                .map(|device| {
                    device
                        .available_commands
                        .iter()
                        .any(|command| command.key == key)
                })
                .unwrap_or(false);
            if !advertised {
                return Err("Remote command was not advertised by this device".to_string());
            }
            let packet = build_remote_command_execute_packet(key)?;
            transport.send(&packet)
        });
    }

    /// Builds a generation-checked transport snapshot under the session lock,
    /// then runs the caller after the lock has been released.  Feature sends
    /// therefore cannot block link replacement, shutdown, or ICE recovery.
    fn with_feature_transport(
        &self,
        device_id: &str,
        f: impl FnOnce(&FeatureTransport) -> Result<(), String>,
    ) {
        let result = FeatureTransport::for_device(&self.sessions, device_id)
            .and_then(|transport| f(&transport));
        if let Err(error) = result {
            if should_surface_send_error(&error) {
                mark_error(&self.devices, device_id, error.clone());
                push_error(&self.errors, error);
            } else {
                eprintln!("[DeskLink] Feature send deferred during WebRTC recovery: {error}");
            }
        }
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

fn build_sftp_request_packet() -> NetworkPacket {
    let mut packet = NetworkPacket::new(PACKET_TYPE_SFTP_REQUEST);
    packet.set("startBrowsing", true);
    packet
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

fn build_system_volume_request_packet() -> NetworkPacket {
    let mut packet = NetworkPacket::new(PACKET_TYPE_SYSTEMVOLUME_REQUEST);
    packet.set("requestSinks", true);
    packet
}

fn build_system_volume_set_packet(
    name: &str,
    volume: Option<i64>,
    muted: Option<bool>,
) -> Result<NetworkPacket, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("Audio sink name is required".to_string());
    }
    if volume.is_none() && muted.is_none() {
        return Err("Volume or mute state is required".to_string());
    }
    let mut packet = NetworkPacket::new(PACKET_TYPE_SYSTEMVOLUME_REQUEST);
    packet.set("name", name);
    if let Some(volume) = volume {
        packet.set("volume", volume.max(0));
    }
    if let Some(muted) = muted {
        packet.set("muted", muted);
    }
    Ok(packet)
}

fn build_remote_command_list_request_packet() -> NetworkPacket {
    let mut packet = NetworkPacket::new(PACKET_TYPE_RUNCOMMAND_REQUEST);
    packet.set("requestCommandList", true);
    packet
}

fn build_remote_command_execute_packet(key: &str) -> Result<NetworkPacket, String> {
    let key = key.trim();
    if key.is_empty() {
        return Err("Remote command key is required".to_string());
    }
    let mut packet = NetworkPacket::new(PACKET_TYPE_RUNCOMMAND_REQUEST);
    packet.set("key", key);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device_links::packet::{
        PACKET_TYPE_MPRIS_REQUEST, PACKET_TYPE_NOTIFICATION_ACTION, PACKET_TYPE_NOTIFICATION_REPLY,
        PACKET_TYPE_NOTIFICATION_REQUEST, PACKET_TYPE_RUNCOMMAND_REQUEST,
        PACKET_TYPE_SHARE_REQUEST, PACKET_TYPE_SYSTEMVOLUME_REQUEST,
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
    fn sftp_request_packet_starts_remote_browsing() {
        let packet = build_sftp_request_packet();

        assert_eq!(packet.packet_type, PACKET_TYPE_SFTP_REQUEST);
        assert_eq!(packet.get_bool("startBrowsing"), Some(true));
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

    #[test]
    fn system_volume_request_packet_requests_sinks() {
        let packet = build_system_volume_request_packet();

        assert_eq!(packet.packet_type, PACKET_TYPE_SYSTEMVOLUME_REQUEST);
        assert_eq!(packet.get_bool("requestSinks"), Some(true));
    }

    #[test]
    fn system_volume_set_packet_requires_a_change() {
        assert!(build_system_volume_set_packet("sink", None, None).is_err());
    }

    #[test]
    fn remote_command_execute_packet_contains_key() {
        let packet = build_remote_command_execute_packet("command-key").unwrap();

        assert_eq!(packet.packet_type, PACKET_TYPE_RUNCOMMAND_REQUEST);
        assert_eq!(packet.get_str("key"), Some("command-key"));
    }
}
