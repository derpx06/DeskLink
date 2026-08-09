use serde_json::Value;
use std::process::Command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sink {
    pub name: String,
    pub description: String,
    pub volume: i64,
    pub muted: bool,
    pub enabled: bool,
}

pub fn list_sinks() -> Result<Vec<Sink>, String> {
    let output = Command::new("pactl")
        .args(["-f", "json", "list", "sinks"])
        .output()
        .map_err(|error| format!("PipeWire/PulseAudio backend unavailable: {error}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    let value: Value = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("Audio backend returned invalid sink data: {error}"))?;
    let sinks = value
        .as_array()
        .ok_or_else(|| "Audio backend returned no sink array".to_string())?
        .iter()
        .filter_map(|sink| {
            let object = sink.as_object()?;
            let name = object.get("name")?.as_str()?.to_string();
            let description = object
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or(&name)
                .to_string();
            let muted = object.get("mute").and_then(Value::as_bool).unwrap_or(false);
            let volume = object
                .get("volume")
                .and_then(Value::as_object)
                .and_then(|channels| channels.values().next())
                .and_then(|channel| channel.get("value"))
                .and_then(Value::as_i64)
                .unwrap_or(0)
                .clamp(0, 65536);
            Some(Sink {
                name,
                description,
                volume: (volume * 100 / 65536).clamp(0, 100),
                muted,
                enabled: true,
            })
        })
        .collect::<Vec<_>>();
    if sinks.is_empty() {
        return Err("Audio backend returned no usable sinks".to_string());
    }
    Ok(sinks)
}

pub fn set_volume(name: &str, volume: i64) -> Result<(), String> {
    validate_sink_name(name)?;
    run_pactl([
        "set-sink-volume",
        name,
        &format!("{}%", volume.clamp(0, 100)),
    ])
}

pub fn set_mute(name: &str, muted: bool) -> Result<(), String> {
    validate_sink_name(name)?;
    run_pactl(["set-sink-mute", name, if muted { "1" } else { "0" }])
}

fn validate_sink_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.chars().any(|character| character.is_control()) {
        Err("Audio sink name is invalid".to_string())
    } else {
        Ok(())
    }
}

fn run_pactl<const N: usize>(args: [&str; N]) -> Result<(), String> {
    let output = Command::new("pactl")
        .args(args)
        .output()
        .map_err(|error| format!("Audio backend unavailable: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}
