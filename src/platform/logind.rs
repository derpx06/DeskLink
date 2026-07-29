use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::OwnedObjectPath;

fn session_id() -> Result<String, String> {
    std::env::var("XDG_SESSION_ID")
        .ok()
        .filter(|id| !id.is_empty())
        .ok_or_else(|| "No active logind session was found".to_string())
}

fn session_path(connection: &Connection, id: &str) -> Result<OwnedObjectPath, String> {
    let manager = Proxy::new(
        connection,
        "org.freedesktop.login1",
        "/org/freedesktop/login1",
        "org.freedesktop.login1.Manager",
    )
    .map_err(|error| error.to_string())?;
    manager
        .call("GetSession", &(id,))
        .map_err(|error| error.to_string())
}

pub fn is_locked() -> Result<bool, String> {
    let connection = Connection::system().map_err(|error| error.to_string())?;
    let path = session_path(&connection, &session_id()?)?;
    let session = Proxy::new(
        &connection,
        "org.freedesktop.login1",
        path.as_str(),
        "org.freedesktop.login1.Session",
    )
    .map_err(|error| error.to_string())?;
    session
        .get_property("LockedHint")
        .map_err(|error| error.to_string())
}

pub fn set_locked(locked: bool) -> Result<(), String> {
    let connection = Connection::system().map_err(|error| error.to_string())?;
    let id = session_id()?;
    let manager = Proxy::new(
        &connection,
        "org.freedesktop.login1",
        "/org/freedesktop/login1",
        "org.freedesktop.login1.Manager",
    )
    .map_err(|error| error.to_string())?;
    if locked {
        manager
            .call::<_, _, ()>("LockSession", &(id,))
            .map_err(|error| error.to_string())
    } else {
        manager
            .call::<_, _, ()>("UnlockSession", &(id,))
            .map_err(|error| error.to_string())
    }
}
