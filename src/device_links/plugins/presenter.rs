use crate::device_links::packet::NetworkPacket;

const MAX_POINTER_DELTA: f64 = 10_000.0;

pub fn pointer_delta(packet: &NetworkPacket) -> Result<Option<(i32, i32)>, String> {
    if packet.get_bool("stop").unwrap_or(false) {
        return Ok(None);
    }
    let dx = packet
        .body
        .get("dx")
        .and_then(|value| value.as_f64())
        .ok_or_else(|| "Presenter packet has no dx value".to_string())?;
    let dy = packet
        .body
        .get("dy")
        .and_then(|value| value.as_f64())
        .ok_or_else(|| "Presenter packet has no dy value".to_string())?;
    if !dx.is_finite()
        || !dy.is_finite()
        || dx.abs() > MAX_POINTER_DELTA
        || dy.abs() > MAX_POINTER_DELTA
    {
        return Err("Presenter pointer delta is outside the allowed range".to_string());
    }
    Ok(Some((dx.round() as i32, dy.round() as i32)))
}

#[cfg(test)]
mod tests {
    use super::pointer_delta;
    use crate::device_links::packet::{NetworkPacket, PACKET_TYPE_PRESENTER};

    #[test]
    fn parses_bounded_pointer_delta() {
        let mut packet = NetworkPacket::new(PACKET_TYPE_PRESENTER);
        packet.set("dx", 4.0);
        packet.set("dy", -3.0);
        assert_eq!(pointer_delta(&packet).unwrap(), Some((4, -3)));
    }

    #[test]
    fn rejects_unbounded_pointer_delta() {
        let mut packet = NetworkPacket::new(PACKET_TYPE_PRESENTER);
        packet.set("dx", 100_001.0);
        packet.set("dy", 0.0);
        assert!(pointer_delta(&packet).is_err());
    }
}
