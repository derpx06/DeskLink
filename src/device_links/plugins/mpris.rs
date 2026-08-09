use crate::device_links::packet::{NetworkPacket, PACKET_TYPE_MPRIS};

pub fn player_list_packet() -> Result<NetworkPacket, String> {
    let players = crate::platform::mpris::players()?;
    let mut packet = NetworkPacket::new(PACKET_TYPE_MPRIS);
    packet.set("playerList", players);
    Ok(packet)
}

pub fn status_packet(player: &str) -> Result<NetworkPacket, String> {
    let values = crate::platform::mpris::status(player)?;
    let mut packet = NetworkPacket::new(PACKET_TYPE_MPRIS);
    for (key, value) in values {
        packet.set(&key, value);
    }
    Ok(packet)
}

pub fn apply_request(packet: &NetworkPacket) -> Result<(), String> {
    let player = packet
        .get_str("player")
        .ok_or_else(|| "Media request has no player".to_string())?;
    if let Some(action) = packet.get_str("action") {
        crate::platform::mpris::action(player, action)?;
    }
    Ok(())
}
