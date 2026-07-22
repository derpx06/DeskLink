use crate::device_links::device::ConnectivityStatus;
use crate::device_links::packet::{NetworkPacket, PACKET_TYPE_CONNECTIVITY_REPORT};

pub fn parse_report(packet: &NetworkPacket) -> Result<ConnectivityStatus, String> {
    if packet.packet_type != PACKET_TYPE_CONNECTIVITY_REPORT {
        return Err("Unexpected connectivity packet type".to_string());
    }
    let report = packet
        .body
        .get("signalStrengths")
        .and_then(|value| value.as_object())
        .ok_or_else(|| "Connectivity report has no signalStrengths object".to_string())?;
    if report.len() > 64 {
        return Err("Connectivity report has too many subscriptions".to_string());
    }
    let mut signal_strengths = Vec::new();
    for (subscription, value) in report {
        let object = value
            .as_object()
            .ok_or_else(|| "Connectivity subscription is not an object".to_string())?;
        let network_type = object
            .get("networkType")
            .and_then(|value| value.as_str())
            .unwrap_or("unknown")
            .to_string();
        let strength = object
            .get("signalStrength")
            .and_then(|value| value.as_i64())
            .ok_or_else(|| "Connectivity signal strength is invalid".to_string())?
            .clamp(-1, 100);
        signal_strengths.push((subscription.clone(), network_type, strength));
    }
    Ok(ConnectivityStatus { signal_strengths })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_signal_strength_report() {
        let mut packet = NetworkPacket::new(PACKET_TYPE_CONNECTIVITY_REPORT);
        packet.body = serde_json::json!({"signalStrengths": {"1": {
            "networkType": "5G", "signalStrength": 4
        }}})
        .as_object()
        .unwrap()
        .clone();
        let report = parse_report(&packet).unwrap();
        assert_eq!(report.signal_strengths[0].1, "5G");
    }
}
