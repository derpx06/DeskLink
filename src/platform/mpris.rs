use zbus::blocking::{Connection, Proxy};

const ROOT_PATH: &str = "/org/mpris/MediaPlayer2";
const PLAYER_INTERFACE: &str = "org.mpris.MediaPlayer2.Player";

pub fn players() -> Result<Vec<String>, String> {
    let connection = Connection::session().map_err(|error| error.to_string())?;
    let proxy = Proxy::new(
        &connection,
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
    )
    .map_err(|error| error.to_string())?;
    let names: Vec<String> = proxy
        .call("ListNames", &())
        .map_err(|error| error.to_string())?;
    Ok(names
        .into_iter()
        .filter(|name| name.starts_with("org.mpris.MediaPlayer2."))
        .collect())
}

pub fn action(player: &str, action: &str) -> Result<(), String> {
    let connection = Connection::session().map_err(|error| error.to_string())?;
    let proxy = Proxy::new(&connection, player, ROOT_PATH, PLAYER_INTERFACE)
        .map_err(|error| error.to_string())?;
    match action {
        "Play" | "Pause" | "PlayPause" | "Stop" | "Next" | "Previous" => proxy
            .call::<_, _, ()>(action, &())
            .map_err(|error| error.to_string()),
        _ => Err("Unsupported media action".to_string()),
    }
}

pub fn status(player: &str) -> Result<serde_json::Map<String, serde_json::Value>, String> {
    let connection = Connection::session().map_err(|error| error.to_string())?;
    let proxy = Proxy::new(&connection, player, ROOT_PATH, PLAYER_INTERFACE)
        .map_err(|error| error.to_string())?;
    let playback: String = proxy
        .get_property("PlaybackStatus")
        .map_err(|error| error.to_string())?;
    let mut result = serde_json::Map::new();
    result.insert("player".to_string(), player.into());
    result.insert("isPlaying".to_string(), (playback == "Playing").into());
    Ok(result)
}

#[allow(dead_code)]
pub fn available() -> bool {
    players()
        .map(|players| !players.is_empty())
        .unwrap_or(false)
}
