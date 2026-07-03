use super::discovery::broadcast_identity;
use super::network::send_packet;
use super::state::{mark_error, push_error, update_pair_state};
use super::{DaemonWorker, Link};
use crate::device_links::packet::{
    NetworkPacket, PACKET_TYPE_FINDMYPHONE_REQUEST, PACKET_TYPE_LOCK_REQUEST,
    PACKET_TYPE_MOUSEPAD_REQUEST, PACKET_TYPE_MPRIS_REQUEST, PACKET_TYPE_NOTIFICATION_ACTION,
    PACKET_TYPE_NOTIFICATION_REPLY, PACKET_TYPE_NOTIFICATION_REQUEST, PACKET_TYPE_PING,
    PACKET_TYPE_RUNCOMMAND_REQUEST, PACKET_TYPE_SFTP_REQUEST, PACKET_TYPE_SHARE_REQUEST,
    PACKET_TYPE_SYSTEMVOLUME_REQUEST,
};
use crate::device_links::pairing::PairState;

#[derive(Debug, Clone)]
pub enum DaemonCommand {
    Discover,
    RequestPair(String),
    AcceptPair(String),
    RejectPair(String),
    Unpair(String),
    SendPing(String),
    SendMousepadRequest(String, serde_json::Map<String, serde_json::Value>),
    SendFile(String, std::path::PathBuf),
    SendShareText(String, String),
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
    Stop,
}

impl DaemonWorker {
    pub(super) fn handle_command(&self, command: DaemonCommand) {
        match command {
            DaemonCommand::Discover => {
                if let Err(error) = broadcast_identity(&self.config, self.tcp_port) {
                    push_error(&self.errors, error);
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
            DaemonCommand::SendFile(id, path) => self.send_file(&id, path),
            DaemonCommand::SendShareText(id, text) => self.send_share_text(&id, &text),
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
            DaemonCommand::Stop => {}
        }
    }
    fn send_pair_request(&self, device_id: &str) {
        self.with_link(device_id, |link| {
            let packet = link.pairing.request_packet();
            send_packet(&link.stream, &packet)?;
            if link.pairing.state == PairState::Paired {
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
                link.pairing.state,
                Some(link.verification_key()),
            );
            Ok(())
        });
    }

    fn accept_pair(&self, device_id: &str) {
        self.with_link(device_id, |link| {
            let packet = link.pairing.accept_packet();
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
            let packet = link.pairing.reject_packet();
            send_packet(&link.stream, &packet)?;
            update_pair_state(&self.devices, device_id, PairState::NotPaired, None);
            Ok(())
        });
    }

    fn unpair(&self, device_id: &str) {
        self.with_link(device_id, |link| {
            let packet = link.pairing.reject_packet();
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
            if link.pairing.state != PairState::Paired {
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
            if link.pairing.state != PairState::Paired {
                return Err("Device must be paired before sending remote control input".to_string());
            }
            let packet = NetworkPacket::with_body(PACKET_TYPE_MOUSEPAD_REQUEST, payload);
            send_packet(&link.stream, &packet)
        });
    }

    fn send_share_text(&self, device_id: &str, text: &str) {
        self.with_link(device_id, |link| {
            if link.pairing.state != PairState::Paired {
                return Err("Device must be paired before sharing text".to_string());
            }
            let packet = build_share_text_packet(text)?;
            send_packet(&link.stream, &packet)
        });
    }

    fn send_lock_request(&self, device_id: &str, lock: bool) {
        self.with_link(device_id, |link| {
            if link.pairing.state != PairState::Paired {
                return Err("Device must be paired before sending lock request".to_string());
            }
            let mut packet = NetworkPacket::new(PACKET_TYPE_LOCK_REQUEST);
            packet.set("setLocked", lock);
            send_packet(&link.stream, &packet)
        });
    }

    fn send_find_phone(&self, device_id: &str) {
        self.with_link(device_id, |link| {
            if link.pairing.state != PairState::Paired {
                return Err("Device must be paired before ringing it".to_string());
            }
            let packet = NetworkPacket::new(PACKET_TYPE_FINDMYPHONE_REQUEST);
            send_packet(&link.stream, &packet)
        });
    }

    fn send_mpris_action(&self, device_id: &str, player: &str, action: &str) {
        self.with_link(device_id, |link| {
            if link.pairing.state != PairState::Paired {
                return Err("Device must be paired before controlling media".to_string());
            }
            let packet = build_mpris_action_packet(player, action);
            send_packet(&link.stream, &packet)
        });
    }

    fn request_mpris_status(&self, device_id: &str, player: Option<String>) {
        self.with_link(device_id, |link| {
            if link.pairing.state != PairState::Paired {
                return Err("Device must be paired before requesting media status".to_string());
            }
            let packet = build_mpris_status_request_packet(player.as_deref());
            send_packet(&link.stream, &packet)
        });
    }

    fn send_mpris_set_volume(&self, device_id: &str, player: &str, volume: i64) {
        self.with_link(device_id, |link| {
            if link.pairing.state != PairState::Paired {
                return Err("Device must be paired before controlling media volume".to_string());
            }
            let packet = build_mpris_set_volume_packet(player, volume)?;
            send_packet(&link.stream, &packet)
        });
    }

    fn send_mpris_seek(&self, device_id: &str, player: &str, offset: i64) {
        self.with_link(device_id, |link| {
            if link.pairing.state != PairState::Paired {
                return Err("Device must be paired before seeking media".to_string());
            }
            let packet = build_mpris_seek_packet(player, offset)?;
            send_packet(&link.stream, &packet)
        });
    }

    fn send_sftp_request(&self, device_id: &str) {
        self.with_link(device_id, |link| {
            if link.pairing.state != PairState::Paired {
                return Err("Device must be paired before requesting file browsing".to_string());
            }
            let packet = build_sftp_request_packet();
            send_packet(&link.stream, &packet)
        });
    }

    fn request_notifications(&self, device_id: &str) {
        self.with_link(device_id, |link| {
            if link.pairing.state != PairState::Paired {
                return Err("Device must be paired before requesting notifications".to_string());
            }
            let packet = build_notification_request_packet();
            send_packet(&link.stream, &packet)
        });
    }

    fn dismiss_notification(&self, device_id: &str, notification_id: &str) {
        self.with_link(device_id, |link| {
            if link.pairing.state != PairState::Paired {
                return Err("Device must be paired before dismissing notifications".to_string());
            }
            let packet = build_notification_dismiss_packet(notification_id)?;
            send_packet(&link.stream, &packet)
        });
    }

    fn reply_notification(&self, device_id: &str, reply_id: &str, message: &str) {
        self.with_link(device_id, |link| {
            if link.pairing.state != PairState::Paired {
                return Err("Device must be paired before replying to notifications".to_string());
            }
            let packet = build_notification_reply_packet(reply_id, message)?;
            send_packet(&link.stream, &packet)
        });
    }

    fn trigger_notification_action(&self, device_id: &str, key: &str, action: &str) {
        self.with_link(device_id, |link| {
            if link.pairing.state != PairState::Paired {
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
            if link.pairing.state != PairState::Paired {
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
            if link.pairing.state != PairState::Paired {
                return Err("Device must be paired before changing system volume".to_string());
            }
            let packet = build_system_volume_set_packet(name, volume, muted)?;
            send_packet(&link.stream, &packet)
        });
    }

    fn request_remote_commands(&self, device_id: &str) {
        self.with_link(device_id, |link| {
            if link.pairing.state != PairState::Paired {
                return Err("Device must be paired before requesting remote commands".to_string());
            }
            let packet = build_remote_command_list_request_packet();
            send_packet(&link.stream, &packet)
        });
    }

    fn execute_remote_command(&self, device_id: &str, key: &str) {
        self.with_link(device_id, |link| {
            if link.pairing.state != PairState::Paired {
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

    pub(super) fn with_link(
        &self,
        device_id: &str,
        f: impl FnOnce(&mut Link) -> Result<(), String>,
    ) {
        let result = self
            .links
            .lock()
            .map_err(|_| "Link lock poisoned".to_string())
            .and_then(|mut links| {
                let link = links
                    .get_mut(device_id)
                    .ok_or_else(|| "Device is not connected".to_string())?;
                f(link)
            });
        if let Err(error) = result {
            mark_error(&self.devices, device_id, error.clone());
            push_error(&self.errors, error);
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
