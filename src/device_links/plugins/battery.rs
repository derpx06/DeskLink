use crate::device_links::packet::{NetworkPacket, PACKET_TYPE_BATTERY};

pub fn local_packet() -> Result<Option<NetworkPacket>, String> {
    let Some(snapshot) = crate::platform::upower::snapshot()? else {
        return Ok(None);
    };
    let mut packet = NetworkPacket::new(PACKET_TYPE_BATTERY);
    packet.set(
        "currentCharge",
        snapshot.percentage.round().clamp(0.0, 100.0) as i64,
    );
    packet.set("isCharging", snapshot.state == 1);
    packet.set("batteryQuantity", 1_i64);
    packet.set("thresholdEvent", 0_i64);
    Ok(Some(packet))
}
