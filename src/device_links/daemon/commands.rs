use super::discovery::broadcast_identity;
use super::network::send_packet;
use super::state::{mark_error, push_error, update_pair_state};
use super::{DaemonWorker, Link};
use crate::device_links::core::device_manager::SessionBinding;
use crate::device_links::core::events::CoreEvent;
use crate::device_links::packet::{
    NetworkPacket, PACKET_TYPE_FINDMYPHONE_REQUEST, PACKET_TYPE_LOCK_REQUEST,
    PACKET_TYPE_MOUSEPAD_REQUEST, PACKET_TYPE_MPRIS_REQUEST, PACKET_TYPE_PING,
    PACKET_TYPE_RUNCOMMAND_REQUEST, PACKET_TYPE_SCREEN_REQUEST, PACKET_TYPE_SCREEN_STOP,
    PACKET_TYPE_SFTP_REQUEST, PACKET_TYPE_SHARE_REQUEST, PACKET_TYPE_SYSTEMVOLUME_REQUEST,
};
use crate::device_links::pairing::PairState;
use crate::device_links::plugins::notifications;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub enum DaemonCommand {
    Discover,
    RequestPair(String),
    AcceptPair(String),
    RejectPair(String),
    Unpair(String),
    SendPing(String),
    SendMousepadRequest(String, serde_json::Map<String, serde_json::Value>),
    SendScreenRequest(String, String),
    SendScreenStop(String),
    SendFile(String, std::path::PathBuf),
    StartTransfer(String, std::path::PathBuf, String),
    CancelTransfer(String),
    SendShareText(String, String),
    SendClipboard(String, String),
    SendLockRequest(String, bool),
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
    RequestContacts(String),
    RequestContactVcards(String, Vec<String>),
    RequestSmsConversations(String),
    RequestSmsConversation(String, String),
    SendTelephonyMute(String),
    SetPreference(String, String),
    Stop,
}

impl DaemonWorker {
    pub(super) fn handle_command(&self, command: DaemonCommand) {
        match command {
            DaemonCommand::Discover => {
                if let Err(error) = broadcast_identity(&self.config, self.tcp_port) {
                    push_error(&self.errors, error);
                    self.events.publish(CoreEvent::Error {
                        scope: "discovery".to_string(),
                        device_id: None,
                        message: "Could not broadcast DeskLink identity".to_string(),
                        retryable: true,
                    });
                }
            }
            DaemonCommand::RequestPair(device_id) => self.send_pair_request(&device_id),
            DaemonCommand::AcceptPair(device_id) => {
                eprintln!("[Daemon] Running accept_pair for {}", device_id);
                self.accept_pair(&device_id);
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
            DaemonCommand::SendScreenRequest(id, role) => self.send_screen_request(&id, &role),
            DaemonCommand::SendScreenStop(id) => self.send_screen_stop(&id),
            DaemonCommand::SendFile(id, path) => self.send_file(&id, path),
            DaemonCommand::StartTransfer(id, path, transfer_id) => {
                self.send_file_with_id(&id, path, transfer_id)
            }
            DaemonCommand::CancelTransfer(transfer_id) => {
                if let Ok(mut cancelled) = self.transfer_cancellations.lock() {
                    cancelled.insert(transfer_id.clone());
                }
                if let Ok(Some(snapshot)) = self.transfer_store.mark_cancelled(&transfer_id) {
                    self.events.publish(CoreEvent::TransferChanged {
                        transfer_id: snapshot.transfer_id,
                        state: "cancelled".to_string(),
                        bytes_done: snapshot.bytes_done,
                        bytes_total: snapshot.bytes_total,
                        can_resume: false,
                        error: None,
                    });
                }
            }
            DaemonCommand::SendShareText(id, text) => self.send_share_text(&id, &text),
            DaemonCommand::SendClipboard(id, text) => self.send_clipboard(&id, &text),
            DaemonCommand::SendLockRequest(id, lock) => self.send_lock_request(&id, lock),
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
            DaemonCommand::RequestContacts(id) => self.send_contacts_request(&id),
            DaemonCommand::RequestContactVcards(id, uids) => {
                self.send_contact_vcards_request(&id, &uids)
            }
            DaemonCommand::RequestSmsConversations(id) => self.send_sms_conversations_request(&id),
            DaemonCommand::RequestSmsConversation(id, thread_id) => {
                self.send_sms_conversation_request(&id, &thread_id)
            }
            DaemonCommand::SendTelephonyMute(id) => self.send_telephony_mute(&id),
            DaemonCommand::SetPreference(key, value) => {
                if let Err(error) = self
                    .config
                    .lock()
                    .map_err(|_| "Config lock poisoned".to_string())
                    .and_then(|mut config| config.set_preference(&key, &value))
                {
                    push_error(
                        &self.errors,
                        format!("Could not save preference {key}: {error}"),
                    );
                    self.events.publish(CoreEvent::Error {
                        scope: "preferences".to_string(),
                        device_id: None,
                        message: format!("Could not save preference {key}: {error}"),
                        retryable: false,
                    });
                }
            }
            DaemonCommand::Stop => {
                self.shutdown
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                for link in self.sessions.terminate_all() {
                    link.close();
                }
            }
        }
    }

    fn binding_for_link(&self, device_id: &str, link: &Link) -> Result<SessionBinding, String> {
        let binding = self
            .sessions
            .current_binding(device_id)
            .ok_or_else(|| "Device is not connected".to_string())?;
        if !self.sessions.is_current(&binding) || !Arc::ptr_eq(&binding.link.stream, &link.stream) {
            return Err("Device session became stale before pairing state was changed".to_string());
        }
        Ok(binding)
    }

    fn pairing_state_for_link(&self, device_id: &str, link: &Link) -> Result<PairState, String> {
        let binding = self.binding_for_link(device_id, link)?;
        self.sessions
            .pairing_state(&binding)
            .map_err(|error| format!("Could not read pairing state: {error:?}"))
    }

    fn pair_packet_for_link(
        &self,
        device_id: &str,
        link: &Link,
        operation: impl FnOnce(&mut crate::device_links::pairing::PairingHandler) -> NetworkPacket,
    ) -> Result<NetworkPacket, String> {
        let binding = self.binding_for_link(device_id, link)?;
        self.sessions
            .with_pairing(&binding, operation)
            .map_err(|error| format!("Could not update pairing state: {error:?}"))
    }

    fn verification_key_for_link(&self, device_id: &str, link: &Link) -> Result<String, String> {
        let binding = self.binding_for_link(device_id, link)?;
        self.sessions
            .verification_key(&binding)
            .map_err(|error| format!("Could not calculate pairing verification key: {error:?}"))
    }

    fn send_pair_request(&self, device_id: &str) {
        self.with_link(device_id, |link| {
            let packet =
                self.pair_packet_for_link(device_id, link, |pairing| pairing.request_packet())?;
            send_packet(&link.stream, &packet)?;
            if self.pairing_state_for_link(device_id, link)? == PairState::Paired {
                self.config
                    .lock()
                    .map_err(|_| "Config lock poisoned".to_string())?
                    .trust_device(&link.info, link.certificate_pem.clone())?;
                update_pair_state(&self.devices, device_id, PairState::Paired, None);
                return Ok(());
            }
            update_pair_state(
                &self.devices,
                device_id,
                self.pairing_state_for_link(device_id, link)?,
                Some(self.verification_key_for_link(device_id, link)?),
            );
            Ok(())
        });
    }

    fn send_contacts_request(&self, device_id: &str) {
        self.with_link(device_id, |link| {
            if self.pairing_state_for_link(device_id, link)? != PairState::Paired {
                return Err("Device must be paired before requesting contacts".to_string());
            }
            send_packet(
                &link.stream,
                &crate::device_links::plugins::contacts::request_all_uids_timestamps(),
            )
        });
    }

    fn send_contact_vcards_request(&self, device_id: &str, uids: &[String]) {
        self.with_link(device_id, |link| {
            let packet = crate::device_links::plugins::contacts::request_vcards(uids)?;
            send_packet(&link.stream, &packet)
        });
    }

    fn send_sms_conversations_request(&self, device_id: &str) {
        self.with_link(device_id, |link| {
            send_packet(
                &link.stream,
                &crate::device_links::plugins::sms::request_conversations(),
            )
        });
    }

    fn send_sms_conversation_request(&self, device_id: &str, thread_id: &str) {
        self.with_link(device_id, |link| {
            let packet = crate::device_links::plugins::sms::request_conversation(thread_id)?;
            send_packet(&link.stream, &packet)
        });
    }

    fn send_telephony_mute(&self, device_id: &str) {
        self.with_link(device_id, |link| {
            send_packet(
                &link.stream,
                &crate::device_links::plugins::telephony::request_mute(),
            )
        });
    }

    fn accept_pair(&self, device_id: &str) {
        self.with_link(device_id, |link| {
            let packet =
                self.pair_packet_for_link(device_id, link, |pairing| pairing.accept_packet())?;
            send_packet(&link.stream, &packet)?;
            self.config
                .lock()
                .map_err(|_| "Config lock poisoned".to_string())?
                .trust_device(&link.info, link.certificate_pem.clone())?;
            update_pair_state(&self.devices, device_id, PairState::Paired, None);
            Ok(())
        });
    }

    fn reject_pair(&self, device_id: &str) {
        self.with_link(device_id, |link| {
            let packet =
                self.pair_packet_for_link(device_id, link, |pairing| pairing.reject_packet())?;
            send_packet(&link.stream, &packet)?;
            update_pair_state(&self.devices, device_id, PairState::NotPaired, None);
            Ok(())
        });
    }

    fn unpair(&self, device_id: &str) {
        self.with_link(device_id, |link| {
            let packet =
                self.pair_packet_for_link(device_id, link, |pairing| pairing.reject_packet())?;
            let _ = send_packet(&link.stream, &packet);
            self.config
                .lock()
                .map_err(|_| "Config lock poisoned".to_string())?
                .untrust_device(device_id)?;
            update_pair_state(&self.devices, device_id, PairState::NotPaired, None);
            Ok(())
        });
    }

    fn send_ping(&self, device_id: &str) {
        self.with_link(device_id, |link| {
            if self.pairing_state_for_link(device_id, link)? != PairState::Paired {
                return Err("Device must be paired before sending ping".to_string());
            }
            let mut packet = NetworkPacket::new(PACKET_TYPE_PING);
            packet.set("message", "Ping from DeskLink");
            send_packet(&link.stream, &packet)
        });
    }

    fn send_mousepad_request(
        &self,
        device_id: &str,
        payload: serde_json::Map<String, serde_json::Value>,
    ) {
        self.with_link(device_id, |link| {
            if self.pairing_state_for_link(device_id, link)? != PairState::Paired {
                return Err("Device must be paired before sending remote control input".to_string());
            }
            let packet = NetworkPacket::with_body(PACKET_TYPE_MOUSEPAD_REQUEST, payload);
            send_packet(&link.stream, &packet)
        });
    }

    fn send_screen_request(&self, device_id: &str, role: &str) {
        self.with_link(device_id, |link| {
            if self.pairing_state_for_link(device_id, link)? != PairState::Paired {
                return Err("Device must be paired before requesting screen sharing".to_string());
            }
            let packet = build_screen_request_packet(role);
            send_packet(&link.stream, &packet)
        });
    }

    fn send_screen_stop(&self, device_id: &str) {
        self.with_link(device_id, |link| {
            if self.pairing_state_for_link(device_id, link)? != PairState::Paired {
                return Err("Device must be paired before stopping screen sharing".to_string());
            }
            let packet = build_screen_stop_packet();
            send_packet(&link.stream, &packet)
        });
    }

    fn send_share_text(&self, device_id: &str, text: &str) {
        self.with_link(device_id, |link| {
            if self.pairing_state_for_link(device_id, link)? != PairState::Paired {
                return Err("Device must be paired before sharing text".to_string());
            }
            let packet = build_share_text_packet(text)?;
            send_packet(&link.stream, &packet)
        });
    }

    fn send_clipboard(&self, device_id: &str, text: &str) {
        self.with_link(device_id, |link| {
            if self.pairing_state_for_link(device_id, link)? != PairState::Paired {
                return Err("Device must be paired before setting the clipboard".to_string());
            }
            if text.is_empty() || text.len() > super::clipboard::MAX_CLIPBOARD_BYTES {
                return Err("Clipboard text is empty or too large".to_string());
            }
            let mut packet = NetworkPacket::new(crate::device_links::packet::PACKET_TYPE_CLIPBOARD);
            packet.set("content", text);
            packet.set("timestamp", current_time_millis());
            send_packet(&link.stream, &packet)
        });
    }

    fn send_lock_request(&self, device_id: &str, lock: bool) {
        self.with_link(device_id, |link| {
            if self.pairing_state_for_link(device_id, link)? != PairState::Paired {
                return Err("Device must be paired before sending lock request".to_string());
            }
            let mut packet = NetworkPacket::new(PACKET_TYPE_LOCK_REQUEST);
            packet.set("setLocked", lock);
            send_packet(&link.stream, &packet)
        });
    }

    fn send_find_phone(&self, device_id: &str) {
        self.with_link(device_id, |link| {
            if self.pairing_state_for_link(device_id, link)? != PairState::Paired {
                return Err("Device must be paired before ringing it".to_string());
            }
            let packet = NetworkPacket::new(PACKET_TYPE_FINDMYPHONE_REQUEST);
            send_packet(&link.stream, &packet)
        });
    }

    fn send_mpris_action(&self, device_id: &str, player: &str, action: &str) {
        self.with_link(device_id, |link| {
            if self.pairing_state_for_link(device_id, link)? != PairState::Paired {
                return Err("Device must be paired before controlling media".to_string());
            }
            let packet = build_mpris_action_packet(player, action);
            send_packet(&link.stream, &packet)
        });
    }

    fn request_mpris_status(&self, device_id: &str, player: Option<String>) {
        self.with_link(device_id, |link| {
            if self.pairing_state_for_link(device_id, link)? != PairState::Paired {
                return Err("Device must be paired before requesting media status".to_string());
            }
            let packet = build_mpris_status_request_packet(player.as_deref());
            send_packet(&link.stream, &packet)
        });
    }

    fn send_mpris_set_volume(&self, device_id: &str, player: &str, volume: i64) {
        self.with_link(device_id, |link| {
            if self.pairing_state_for_link(device_id, link)? != PairState::Paired {
                return Err("Device must be paired before controlling media volume".to_string());
            }
            let packet = build_mpris_set_volume_packet(player, volume)?;
            send_packet(&link.stream, &packet)
        });
    }

    fn send_mpris_seek(&self, device_id: &str, player: &str, offset: i64) {
        self.with_link(device_id, |link| {
            if self.pairing_state_for_link(device_id, link)? != PairState::Paired {
                return Err("Device must be paired before seeking media".to_string());
            }
            let packet = build_mpris_seek_packet(player, offset)?;
            send_packet(&link.stream, &packet)
        });
    }

    fn send_sftp_request(&self, device_id: &str) {
        self.with_link(device_id, |link| {
            if self.pairing_state_for_link(device_id, link)? != PairState::Paired {
                return Err("Device must be paired before requesting file browsing".to_string());
            }
            let packet = build_sftp_request_packet();
            send_packet(&link.stream, &packet)
        });
    }

    fn request_notifications(&self, device_id: &str) {
        self.with_link(device_id, |link| {
            if self.pairing_state_for_link(device_id, link)? != PairState::Paired {
                return Err("Device must be paired before requesting notifications".to_string());
            }
            let packet = build_notification_request_packet();
            send_packet(&link.stream, &packet)
        });
    }

    fn dismiss_notification(&self, device_id: &str, notification_id: &str) {
        self.with_link(device_id, |link| {
            if self.pairing_state_for_link(device_id, link)? != PairState::Paired {
                return Err("Device must be paired before dismissing notifications".to_string());
            }
            let packet = build_notification_dismiss_packet(notification_id)?;
            send_packet(&link.stream, &packet)
        });
    }

    fn reply_notification(&self, device_id: &str, reply_id: &str, message: &str) {
        self.with_link(device_id, |link| {
            if self.pairing_state_for_link(device_id, link)? != PairState::Paired {
                return Err("Device must be paired before replying to notifications".to_string());
            }
            let packet = build_notification_reply_packet(reply_id, message)?;
            send_packet(&link.stream, &packet)
        });
    }

    fn trigger_notification_action(&self, device_id: &str, key: &str, action: &str) {
        self.with_link(device_id, |link| {
            if self.pairing_state_for_link(device_id, link)? != PairState::Paired {
                return Err(
                    "Device must be paired before triggering notification actions".to_string(),
                );
            }
            let packet = build_notification_action_packet(key, action)?;
            send_packet(&link.stream, &packet)
        });
    }

    fn request_system_volume(&self, device_id: &str) {
        self.with_link(device_id, |link| {
            if self.pairing_state_for_link(device_id, link)? != PairState::Paired {
                return Err("Device must be paired before requesting system volume".to_string());
            }
            let packet = build_system_volume_request_packet();
            send_packet(&link.stream, &packet)
        });
    }

    fn set_system_volume(
        &self,
        device_id: &str,
        name: &str,
        volume: Option<i64>,
        muted: Option<bool>,
    ) {
        self.with_link(device_id, |link| {
            if self.pairing_state_for_link(device_id, link)? != PairState::Paired {
                return Err("Device must be paired before changing system volume".to_string());
            }
            let packet = build_system_volume_set_packet(name, volume, muted)?;
            send_packet(&link.stream, &packet)
        });
    }

    fn request_remote_commands(&self, device_id: &str) {
        self.with_link(device_id, |link| {
            if self.pairing_state_for_link(device_id, link)? != PairState::Paired {
                return Err("Device must be paired before requesting remote commands".to_string());
            }
            let packet = build_remote_command_list_request_packet();
            send_packet(&link.stream, &packet)
        });
    }

    fn execute_remote_command(&self, device_id: &str, key: &str) {
        self.with_link(device_id, |link| {
            if self.pairing_state_for_link(device_id, link)? != PairState::Paired {
                return Err("Device must be paired before executing remote commands".to_string());
            }
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
            send_packet(&link.stream, &packet)
        });
    }

    pub(super) fn with_link(&self, device_id: &str, f: impl FnOnce(&Link) -> Result<(), String>) {
        let result = self
            .sessions
            .current_binding(device_id)
            .ok_or_else(|| "Device is not connected".to_string())
            .and_then(|binding| {
                if !self.sessions.is_current(&binding) {
                    return Err(
                        "Device session became stale before the command was sent".to_string()
                    );
                }
                f(&binding.link)
            });
        if let Err(error) = result {
            mark_error(&self.devices, device_id, error.clone());
            push_error(&self.errors, error.clone());
            self.events.publish(CoreEvent::Error {
                scope: "command".to_string(),
                device_id: Some(device_id.to_string()),
                message: error,
                retryable: true,
            });
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
        crate::platform::url::validate_http_url(text)?;
        packet.set("url", text);
    } else {
        packet.set("text", text);
    }
    Ok(packet)
}

fn current_time_millis() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default()
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

fn build_screen_request_packet(role: &str) -> NetworkPacket {
    let mut packet = NetworkPacket::new(PACKET_TYPE_SCREEN_REQUEST);
    packet.set("role", role);
    packet.set("maxDimension", 1280);
    packet.set("fps", 6);
    packet.set("quality", 60);
    packet
}

fn build_screen_stop_packet() -> NetworkPacket {
    NetworkPacket::new(PACKET_TYPE_SCREEN_STOP)
}

fn build_notification_request_packet() -> NetworkPacket {
    notifications::request_packet()
}

fn build_notification_dismiss_packet(notification_id: &str) -> Result<NetworkPacket, String> {
    notifications::cancel_packet(notification_id)
}

fn build_notification_reply_packet(reply_id: &str, message: &str) -> Result<NetworkPacket, String> {
    notifications::reply_packet(reply_id, message)
}

fn build_notification_action_packet(key: &str, action: &str) -> Result<NetworkPacket, String> {
    notifications::action_packet(key, action)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device_links::packet::{
        PACKET_TYPE_MPRIS_REQUEST, PACKET_TYPE_NOTIFICATION_ACTION, PACKET_TYPE_NOTIFICATION_REPLY,
        PACKET_TYPE_NOTIFICATION_REQUEST, PACKET_TYPE_RUNCOMMAND_REQUEST,
        PACKET_TYPE_SCREEN_REQUEST, PACKET_TYPE_SCREEN_STOP, PACKET_TYPE_SHARE_REQUEST,
        PACKET_TYPE_SYSTEMVOLUME_REQUEST,
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

    #[test]
    fn screen_request_packet_asks_for_phone_screen_with_default_lan_quality() {
        let packet = build_screen_request_packet("phone-screen");

        assert_eq!(packet.packet_type, PACKET_TYPE_SCREEN_REQUEST);
        assert_eq!(packet.get_str("role"), Some("phone-screen"));
        assert_eq!(packet.get_i64("maxDimension"), Some(1280));
        assert_eq!(packet.get_i64("fps"), Some(6));
        assert_eq!(packet.get_i64("quality"), Some(60));
    }

    #[test]
    fn screen_stop_packet_uses_screen_stop_type() {
        let packet = build_screen_stop_packet();

        assert_eq!(packet.packet_type, PACKET_TYPE_SCREEN_STOP);
    }
}
