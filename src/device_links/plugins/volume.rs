use serde_json::json;

use crate::device_links::packet::{NetworkPacket, PACKET_TYPE_SYSTEMVOLUME};

pub fn status_packet() -> Result<NetworkPacket, String> {
    let sinks = crate::platform::audio::list_sinks()?;
    let sink_list = sinks
        .into_iter()
        .map(|sink| {
            json!({
                "name": sink.name,
                "description": sink.description,
                "volume": sink.volume,
                "maxVolume": 100,
                "muted": sink.muted,
                "enabled": sink.enabled,
            })
        })
        .collect();
    let mut packet = NetworkPacket::new(PACKET_TYPE_SYSTEMVOLUME);
    packet.set("sinkList", serde_json::Value::Array(sink_list));
    Ok(packet)
}

pub fn apply_request(packet: &NetworkPacket) -> Result<(), String> {
    let name = packet
        .get_str("name")
        .ok_or_else(|| "Volume request has no sink name".to_string())?;
    if let Some(volume) = packet.get_i64("volume") {
        crate::platform::audio::set_volume(name, volume)?;
    }
    if let Some(muted) = packet.get_bool("muted") {
        crate::platform::audio::set_mute(name, muted)?;
    }
    if packet.get_bool("enabled") == Some(true) {
        // The PulseAudio protocol has no portable "default sink" property in
        // a sink packet. Setting the default is a separate explicit command.
        let output = std::process::Command::new("pactl")
            .args(["set-default-sink", name])
            .output()
            .map_err(|error| format!("Audio backend unavailable: {error}"))?;
        if !output.status.success() {
            return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
        }
    }
    Ok(())
}
