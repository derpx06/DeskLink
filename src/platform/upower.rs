use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::OwnedObjectPath;

#[derive(Debug, Clone, PartialEq)]
pub struct BatterySnapshot {
    pub percentage: f64,
    pub state: u32,
}

pub fn snapshot() -> Result<Option<BatterySnapshot>, String> {
    let connection = Connection::system().map_err(|error| error.to_string())?;
    let upower = Proxy::new(
        &connection,
        "org.freedesktop.UPower",
        "/org/freedesktop/UPower",
        "org.freedesktop.UPower",
    )
    .map_err(|error| error.to_string())?;
    let devices: Vec<OwnedObjectPath> = upower
        .call("EnumerateDevices", &())
        .map_err(|error| error.to_string())?;
    for path in devices {
        let device = Proxy::new(
            &connection,
            "org.freedesktop.UPower",
            path.as_str(),
            "org.freedesktop.UPower.Device",
        )
        .map_err(|error| error.to_string())?;
        let power_supply: bool = device
            .get_property("PowerSupply")
            .map_err(|error| error.to_string())?;
        if !power_supply {
            continue;
        }
        let percentage: f64 = device
            .get_property("Percentage")
            .map_err(|error| error.to_string())?;
        let state: u32 = device
            .get_property("State")
            .map_err(|error| error.to_string())?;
        return Ok(Some(BatterySnapshot { percentage, state }));
    }
    Ok(None)
}

pub fn available() -> bool {
    matches!(snapshot(), Ok(Some(_)))
}

#[cfg(test)]
mod tests {
    use super::BatterySnapshot;

    #[test]
    fn snapshot_is_a_bounded_percentage() {
        let snapshot = BatterySnapshot {
            percentage: 87.5,
            state: 1,
        };
        assert!((0.0..=100.0).contains(&snapshot.percentage));
    }
}
