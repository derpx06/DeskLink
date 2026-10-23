//! The desk's music on the phone: a client for the local MPRIS players
//! (`org.mpris.MediaPlayer2.*` on the session bus).
//!
//! The players are read once a second; a change in what the phone shows
//! (player list, track, state) is pushed to every connected phone at once,
//! and the position every five seconds while something plays. With no
//! players, `playerList: []` is sent once. The phone's requests
//! (`desklink.mpris.request`) play, pause, skip, seek or set the volume.
//!
//! ponytail: polled rather than watching NameOwnerChanged and
//! PropertiesChanged — at most one push a second either way, and no main
//! loop on this thread; switch to signals if a second of lag is noticed.
//!
//! [`state_packet`] and [`metadata`] decide what the phone is told and are
//! unit-tested with made-up players.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use gtk::gio;
use gtk::glib;
use gtk::prelude::*;

use super::packet::{NetworkPacket, PACKET_TYPE_MPRIS};

const ROOT: &str = "org.mpris.MediaPlayer2";
const PLAYER: &str = "org.mpris.MediaPlayer2.Player";
const PATH: &str = "/org/mpris/MediaPlayer2";
/// How often a playing track's position is resent.
const POSITION_EVERY: Duration = Duration::from_secs(5);

/// One local player as the phone sees it.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Player {
    /// Its bus name, for calls.
    pub(crate) bus_name: String,
    /// Its name for people ("Spotify"), what the phone calls it.
    pub(crate) name: String,
    pub(crate) title: String,
    pub(crate) artist: String,
    pub(crate) album: String,
    pub(crate) length_ms: i64,
    pub(crate) position_ms: i64,
    pub(crate) playing: bool,
    pub(crate) can_play: bool,
    pub(crate) can_pause: bool,
    pub(crate) can_go_next: bool,
    pub(crate) can_go_previous: bool,
    pub(crate) can_seek: bool,
    /// 0..100.
    pub(crate) volume: i64,
    pub(crate) art_url: Option<String>,
    /// MPRIS track id, for SetPosition.
    pub(crate) track_id: Option<String>,
}

/// `desklink.mpris` for the phone: the list, and the active player's state.
pub(crate) fn state_packet(players: &[Player], active: Option<&Player>) -> NetworkPacket {
    let mut packet = NetworkPacket::new(PACKET_TYPE_MPRIS);
    packet.set(
        "playerList",
        players
            .iter()
            .map(|player| player.name.clone())
            .collect::<Vec<_>>(),
    );
    if let Some(player) = active {
        packet.set("player", player.name.as_str());
        packet.set("title", player.title.as_str());
        packet.set("artist", player.artist.as_str());
        packet.set("album", player.album.as_str());
        packet.set("length", player.length_ms);
        packet.set("pos", player.position_ms);
        packet.set("isPlaying", player.playing);
        packet.set("canPlay", player.can_play);
        packet.set("canPause", player.can_pause);
        packet.set("canGoNext", player.can_go_next);
        packet.set("canGoPrevious", player.can_go_previous);
        packet.set("canSeek", player.can_seek);
        packet.set("volume", player.volume);
        if let Some(art) = &player.art_url {
            packet.set("albumArtUrl", art.as_str());
        }
    }
    packet
}

/// The active player: the one playing, else the one that played last.
fn active<'a>(
    players: &'a [Player],
    last_playing: &HashMap<String, Instant>,
) -> Option<&'a Player> {
    players.iter().find(|player| player.playing).or_else(|| {
        players
            .iter()
            .max_by_key(|player| last_playing.get(&player.bus_name).copied())
    })
}

/// Title, artist, album, length (ms), art URL and track id from MPRIS
/// `Metadata` (`a{sv}`).
pub(crate) fn metadata(
    value: &glib::Variant,
) -> (String, String, String, i64, Option<String>, Option<String>) {
    let dict =
        glib::VariantDict::new(Some(value).filter(|v| v.type_() == glib::VariantTy::VARDICT));
    let text = |key: &str| {
        dict.lookup::<String>(key)
            .ok()
            .flatten()
            .unwrap_or_default()
    };
    let artist = dict
        .lookup::<Vec<String>>("xesam:artist")
        .ok()
        .flatten()
        .map(|artists| artists.join(", "))
        .unwrap_or_default();
    let length_us = dict
        .lookup_value("mpris:length", None)
        .and_then(|length| {
            length
                .get::<i64>()
                .or_else(|| length.get::<u64>().map(|v| v as i64))
        })
        .unwrap_or(0);
    let track = dict
        .lookup_value("mpris:trackid", None)
        .and_then(|id| id.str().map(str::to_string));
    let art = dict
        .lookup::<String>("mpris:artUrl")
        .ok()
        .flatten()
        .filter(|url| !url.is_empty());
    let page = dict.lookup::<String>("xesam:url").ok().flatten();
    let art = usable_art(art, page.as_deref(), local_art);
    (
        text("xesam:title"),
        artist,
        text("xesam:album"),
        length_us / 1000,
        art,
        track,
    )
}

/// The art URL to read: the player's own; a local file this process cannot
/// see at its path is looked up with `find` (a Flatpak app's private /tmp);
/// failing that, a YouTube video's thumbnail from the page URL, else none.
fn usable_art(
    art: Option<String>,
    page: Option<&str>,
    find: impl Fn(&std::path::Path) -> Option<std::path::PathBuf>,
) -> Option<String> {
    art.and_then(|url| {
        if !url.to_ascii_lowercase().starts_with("file://") {
            return Some(url);
        }
        let path = gio::File::for_uri(&url).path()?;
        find(&path).map(|found| gio::File::for_path(found).uri().to_string())
    })
    .or_else(|| page.and_then(youtube_thumbnail))
}

/// Where a player's local art file really is. A Flatpak app (Chrome, for
/// one) writes `/tmp/<name>` in its own /tmp, which the host sees as
/// `$XDG_RUNTIME_DIR/.flatpak/<app>/tmp/<name>`; only a file directly in
/// /tmp is looked for there.
fn local_art(path: &std::path::Path) -> Option<std::path::PathBuf> {
    if path.is_file() {
        return Some(path.to_path_buf());
    }
    let name = path.file_name()?;
    if path.parent() != Some(std::path::Path::new("/tmp")) {
        return None;
    }
    std::fs::read_dir(glib::user_runtime_dir().join(".flatpak"))
        .ok()?
        .flatten()
        .map(|app| app.path().join("tmp").join(name))
        .find(|candidate| candidate.is_file())
}

/// `https://i.ytimg.com/vi/<id>/hqdefault.jpg` for a YouTube video page:
/// youtube.com (www., m., music.) `/watch?v=<id>`, `/shorts/<id>`,
/// `/embed/<id>`, `/live/<id>`, or `youtu.be/<id>`.
fn youtube_thumbnail(page: &str) -> Option<String> {
    let rest = page
        .strip_prefix("https://")
        .or_else(|| page.strip_prefix("http://"))?;
    let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
    let host = host.to_ascii_lowercase();
    let (path, query) = path.split_once('?').unwrap_or((path, ""));
    let path = path.split('#').next().unwrap_or_default();
    let id = match host.as_str() {
        "youtu.be" => path.split('/').next()?,
        "youtube.com" | "www.youtube.com" | "m.youtube.com" | "music.youtube.com" => {
            match path.split_once('/') {
                Some(("shorts" | "embed" | "live", id)) => id.split('/').next()?,
                _ if path == "watch" => query
                    .split(['&', '#'])
                    .find_map(|pair| pair.strip_prefix("v="))?,
                _ => return None,
            }
        }
        _ => return None,
    };
    let valid = id.len() == 11
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    valid.then(|| format!("https://i.ytimg.com/vi/{id}/hqdefault.jpg"))
}

/// Every property of `interface` on player `name`, in one call.
fn all_properties(bus: &gio::DBusConnection, name: &str, interface: &str) -> glib::VariantDict {
    let reply = bus
        .call_sync(
            Some(name),
            PATH,
            "org.freedesktop.DBus.Properties",
            "GetAll",
            Some(&(interface,).to_variant()),
            None,
            gio::DBusCallFlags::NONE,
            500,
            gio::Cancellable::NONE,
        )
        .ok();
    glib::VariantDict::new(
        reply
            .map(|reply| reply.child_value(0))
            .filter(|v| v.type_() == glib::VariantTy::VARDICT)
            .as_ref(),
    )
}

fn read_player(bus: &gio::DBusConnection, bus_name: &str) -> Option<Player> {
    let root = all_properties(bus, bus_name, ROOT);
    let player = all_properties(bus, bus_name, PLAYER);
    let flag = |key: &str| player.lookup::<bool>(key).ok().flatten().unwrap_or(false);
    let name = root
        .lookup::<String>("Identity")
        .ok()
        .flatten()
        .unwrap_or_else(|| {
            bus_name
                .trim_start_matches("org.mpris.MediaPlayer2.")
                .to_string()
        });
    let (title, artist, album, length_ms, art_url, track_id) = player
        .lookup_value("Metadata", None)
        .map(|value| metadata(&value))
        .unwrap_or_default();
    Some(Player {
        bus_name: bus_name.to_string(),
        name,
        title,
        artist,
        album,
        length_ms,
        position_ms: player
            .lookup::<i64>("Position")
            .ok()
            .flatten()
            .map_or(0, |us| us / 1000),
        playing: player
            .lookup::<String>("PlaybackStatus")
            .ok()
            .flatten()
            .is_some_and(|status| status == "Playing"),
        can_play: flag("CanPlay"),
        can_pause: flag("CanPause"),
        can_go_next: flag("CanGoNext"),
        can_go_previous: flag("CanGoPrevious"),
        can_seek: flag("CanSeek"),
        volume: player
            .lookup::<f64>("Volume")
            .ok()
            .flatten()
            .map_or(100, |volume| {
                (volume * 100.0).round().clamp(0.0, 100.0) as i64
            }),
        art_url,
        track_id,
    })
}

/// `playerctld` re-exports whichever player is active: listing it as well
/// showed every player twice, and acting through it raced the real one.
const PROXIES: [&str; 1] = ["org.mpris.MediaPlayer2.playerctld"];

/// The MPRIS players among the session bus's names, proxies left out.
fn player_names(names: &[String]) -> Vec<&str> {
    names
        .iter()
        .map(String::as_str)
        .filter(|name| name.starts_with("org.mpris.MediaPlayer2."))
        .filter(|name| !PROXIES.contains(name))
        // DeskLink's own exports of phone players: never echoed to a phone.
        .filter(|name| !name.starts_with(crate::phone_mpris::BUS_PREFIX))
        .collect()
}

fn players(bus: &gio::DBusConnection) -> Vec<Player> {
    let Ok(reply) = bus.call_sync(
        Some("org.freedesktop.DBus"),
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
        "ListNames",
        None,
        None,
        gio::DBusCallFlags::NONE,
        1000,
        gio::Cancellable::NONE,
    ) else {
        return Vec::new();
    };
    let names = reply
        .child_value(0)
        .get::<Vec<String>>()
        .unwrap_or_default();
    let mut players: Vec<Player> = player_names(&names)
        .into_iter()
        .filter_map(|name| read_player(bus, name))
        .collect();
    players.sort_by(|a, b| a.name.cmp(&b.name));
    players
}

/// Pauses every player that is playing, for a phone call; returns their
/// bus names, the only ones played again after it.
pub(crate) fn pause_playing() -> Vec<String> {
    let Ok(bus) = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE) else {
        return Vec::new();
    };
    players(&bus)
        .into_iter()
        .filter(|player| player.playing && player.can_pause)
        .filter(|player| call(&bus, &player.bus_name, "Pause", None).is_ok())
        .map(|player| player.bus_name)
        .collect()
}

/// Plays `bus_names` again after a call (a player that quit is skipped).
pub(crate) fn play(bus_names: &[String]) {
    if bus_names.is_empty() {
        return;
    }
    let Ok(bus) = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE) else {
        return;
    };
    for name in bus_names {
        let _ = call(&bus, name, "Play", None);
    }
}

/// What was last sent, and when a playing track's position was.
#[derive(Default)]
struct Shown {
    key: Option<String>,
    position_sent: Option<Instant>,
    last_playing: HashMap<String, Instant>,
    players: Vec<Player>,
}

static SHOWN: Mutex<Option<Shown>> = Mutex::new(None);

/// How long the player reader waits for a player's own signal before it
/// looks anyway (players that do not announce their changes): short while
/// something plays, long while players only sit there, longest with none.
fn look_again_after(any_playing: bool, any_player: bool) -> Duration {
    if any_playing {
        Duration::from_secs(2)
    } else if any_player {
        Duration::from_secs(10)
    } else {
        Duration::from_secs(30)
    }
}

/// Starts reading the local players, once: when a player says something
/// changed (or appears or quits), and otherwise at the pace
/// [`look_again_after`] gives.
pub(crate) fn start() {
    static STARTED: std::sync::Once = std::sync::Once::new();
    STARTED.call_once(|| {
        let _ = std::thread::Builder::new()
            .name("desklink-mpris".into())
            .spawn(|| {
                let Ok(bus) = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE)
                else {
                    return;
                };
                let (changed, signals) = std::sync::mpsc::channel::<()>();
                // Kept for as long as the thread lives.
                let _subscriptions = [
                    // A player starts or quits.
                    bus.signal_subscribe(
                        Some("org.freedesktop.DBus"),
                        Some("org.freedesktop.DBus"),
                        Some("NameOwnerChanged"),
                        Some("/org/freedesktop/DBus"),
                        Some(ROOT),
                        gio::DBusSignalFlags::MATCH_ARG0_NAMESPACE,
                        {
                            let changed = changed.clone();
                            move |_, _, _, _, _, _| {
                                let _ = changed.send(());
                            }
                        },
                    ),
                    // A player's state, track or volume changes.
                    bus.signal_subscribe(
                        None,
                        Some("org.freedesktop.DBus.Properties"),
                        Some("PropertiesChanged"),
                        Some(PATH),
                        None,
                        gio::DBusSignalFlags::NONE,
                        move |_, _, _, _, _, _| {
                            let _ = changed.send(());
                        },
                    ),
                ];
                loop {
                    poll(&bus);
                    let (playing, any) = SHOWN
                        .lock()
                        .ok()
                        .and_then(|shown| {
                            shown.as_ref().map(|shown| {
                                (
                                    shown.players.iter().any(|player| player.playing),
                                    !shown.players.is_empty(),
                                )
                            })
                        })
                        .unwrap_or_default();
                    match signals.recv_timeout(look_again_after(playing, any)) {
                        // A burst of signals is one look, a moment later.
                        Ok(()) => {
                            std::thread::sleep(Duration::from_millis(150));
                            while signals.try_recv().is_ok() {}
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
            });
    });
}

fn poll(bus: &gio::DBusConnection) {
    let players = players(bus);
    let packet = {
        let Ok(mut shown) = SHOWN.lock() else {
            return;
        };
        let shown = shown.get_or_insert_with(Shown::default);
        let now = Instant::now();
        for player in players.iter().filter(|player| player.playing) {
            shown.last_playing.insert(player.bus_name.clone(), now);
        }
        // Players that quit are forgotten.
        shown
            .last_playing
            .retain(|bus_name, _| players.iter().any(|player| &player.bus_name == bus_name));
        let current = active(&players, &shown.last_playing).cloned();
        let art = current.as_ref().and_then(current_art);
        // What the phone shows, without the position: a change sends now.
        let key = format!(
            "{:?}|{:?}|{:?}",
            players.iter().map(|p| &p.name).collect::<Vec<_>>(),
            current
                .as_ref()
                .map(|p| (&p.name, &p.title, &p.artist, p.playing, p.volume, p.can_seek)),
            art.as_ref().map(|art| &art.id)
        );
        let position_due = current.as_ref().is_some_and(|player| player.playing)
            && shown
                .position_sent
                .is_none_or(|at| now.duration_since(at) >= POSITION_EVERY);
        shown.players = players.clone();
        if shown.key.as_ref() == Some(&key) && !position_due {
            None
        } else {
            shown.key = Some(key);
            shown.position_sent = Some(now);
            Some((state_packet(&players, current.as_ref()), art))
        }
    };
    if let Some((packet, art)) = packet {
        for device in super::desk_features::ready_devices() {
            send_with_art(&device, packet.clone(), art.as_deref());
        }
    }
}

/// The current state with its artwork, if there is one: the packet carries
/// its `rev` (the position, which moves every second, does not count).
pub(crate) fn state_snapshot(
) -> Option<(NetworkPacket, Option<std::sync::Arc<super::media_art::Art>>)> {
    let shown = SHOWN.lock().ok()?;
    let shown = shown.as_ref()?;
    let current = active(&shown.players, &shown.last_playing);
    let mut packet = state_packet(&shown.players, current);
    let rev = super::sync::rev_of(std::slice::from_ref(&packet), &["pos"]);
    packet.set("rev", rev);
    Some((packet, current.and_then(current_art)))
}

/// Sends a [`state_snapshot`] to `device_id`.
pub(crate) fn send_snapshot_to(
    device_id: &str,
    snapshot: (NetworkPacket, Option<std::sync::Arc<super::media_art::Art>>),
) {
    send_with_art(device_id, snapshot.0, snapshot.1.as_deref());
}

/// Sends `device_id` the current state (when it asks).
pub(crate) fn send_state_to(device_id: &str) {
    if let Some(snapshot) = state_snapshot() {
        send_snapshot_to(device_id, snapshot);
    }
}

/// The art id each phone was last sent the picture for.
static ART_SENT: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);

/// `player`'s artwork, when ready (reading it starts here; once done, the
/// next poll pushes it).
fn current_art(player: &Player) -> Option<std::sync::Arc<super::media_art::Art>> {
    super::media_art::art_for(player.art_url.as_deref()?, || {
        if let Ok(mut shown) = SHOWN.lock() {
            if let Some(shown) = shown.as_mut() {
                shown.key = None;
            }
        }
    })
}

/// Adds the artwork to `packet`: the picture itself only when `include_data`
/// (the phone does not have this id yet), else just its id.
pub(crate) fn add_art(
    packet: &mut NetworkPacket,
    art: Option<&super::media_art::Art>,
    include_data: bool,
) {
    if let Some(art) = art {
        packet.set("albumArtId", art.id.as_str());
        if include_data {
            packet.set("albumArtData", art.data.as_str());
        }
    }
}

/// Sends `packet` with the artwork, the picture only if this phone has not
/// been sent it.
fn send_with_art(device_id: &str, mut packet: NetworkPacket, art: Option<&super::media_art::Art>) {
    let include_data = art.is_some_and(|art| {
        ART_SENT.lock().is_ok_and(|mut sent| {
            let sent = sent.get_or_insert_with(HashMap::new);
            let new = sent.get(device_id) != Some(&art.id);
            sent.insert(device_id.to_string(), art.id.clone());
            new
        })
    });
    add_art(&mut packet, art, include_data);
    super::desk_features::send(device_id, &packet);
}

/// A phone (re)connected: it gets the picture again with the next state.
pub(crate) fn forget_art_sent(device_id: &str) {
    if let Ok(mut sent) = ART_SENT.lock() {
        if let Some(sent) = sent.as_mut() {
            sent.remove(device_id);
        }
    }
}

/// The player a request acts on: the one it names; else (or when that one
/// has gone) the one playing, else the one that played last, else the first.
fn target<'a>(
    players: &'a [Player],
    requested: Option<&str>,
    last_playing: &HashMap<String, Instant>,
) -> Option<&'a Player> {
    if let Some(name) = requested {
        if let Some(player) = players.iter().find(|player| player.name == name) {
            return Some(player);
        }
        debug_log!(
            "[DeskLink] Media request for {name:?}, which is not running; using the active player"
        );
    }
    active(players, last_playing).or(players.first())
}

/// A phone's `desklink.mpris.request` for the desk's players. After a
/// change the phone gets the new state at once, and again a moment later:
/// players apply some changes asynchronously.
pub(crate) fn handle_request(device_id: &str, packet: &NetworkPacket) -> Result<(), String> {
    let bus = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE)
        .map_err(|error| error.to_string())?;
    let players = players(&bus);
    let last_playing = SHOWN
        .lock()
        .ok()
        .and_then(|shown| shown.as_ref().map(|shown| shown.last_playing.clone()))
        .unwrap_or_default();
    let target = target(&players, packet.get_str("player"), &last_playing);
    let mut changed = false;
    if let (Some(player), Some(action)) = (target, packet.get_str("action")) {
        if !matches!(
            action,
            "Play" | "Pause" | "PlayPause" | "Next" | "Previous" | "Stop"
        ) {
            return Err(format!("Unknown media action {action}"));
        }
        call(&bus, &player.bus_name, action, None)?;
        changed = true;
    }
    if let (Some(player), Some(offset_us)) = (target, packet.get_i64("Seek")) {
        call(&bus, &player.bus_name, "Seek", Some(offset_us.to_variant()))?;
        changed = true;
    }
    if let (Some(player), Some(position_ms)) = (target, packet.get_i64("SetPosition")) {
        if let Some(track) = &player.track_id {
            let path = glib::variant::ObjectPath::try_from(track.as_str())
                .map_err(|error| error.to_string())?;
            call(
                &bus,
                &player.bus_name,
                "SetPosition",
                Some((path, position_ms.max(0).saturating_mul(1000)).to_variant()),
            )?;
            changed = true;
        }
    }
    if let (Some(player), Some(volume)) = (target, packet.get_i64("setVolume")) {
        let level = (volume.clamp(0, 100) as f64) / 100.0;
        bus.call_sync(
            Some(&player.bus_name),
            PATH,
            "org.freedesktop.DBus.Properties",
            "Set",
            Some(&(PLAYER, "Volume", level.to_variant()).to_variant()),
            None,
            gio::DBusCallFlags::NONE,
            1000,
            gio::Cancellable::NONE,
        )
        .map_err(|error| error.to_string())?;
        changed = true;
    }
    // The state now (asked for, or just changed), not at the next poll.
    refresh_and_send(&bus, device_id);
    if changed {
        std::thread::sleep(SETTLE);
        refresh_and_send(&bus, device_id);
    }
    Ok(())
}

/// How long after a change the state is read again.
const SETTLE: Duration = Duration::from_millis(300);

/// Reads the players now and sends `device_id` their state; the next poll
/// tells the other phones.
fn refresh_and_send(bus: &gio::DBusConnection, device_id: &str) {
    let players = self::players(bus);
    if let Ok(mut shown) = SHOWN.lock() {
        let shown = shown.get_or_insert_with(Shown::default);
        let now = Instant::now();
        for player in players.iter().filter(|player| player.playing) {
            shown.last_playing.insert(player.bus_name.clone(), now);
        }
        shown.players = players;
        shown.key = None;
    }
    send_state_to(device_id);
}

fn call(
    bus: &gio::DBusConnection,
    name: &str,
    method: &str,
    argument: Option<glib::Variant>,
) -> Result<(), String> {
    let parameters = argument.map(|argument| glib::Variant::tuple_from_iter([argument]));
    bus.call_sync(
        Some(name),
        PATH,
        PLAYER,
        method,
        parameters.as_ref(),
        None,
        gio::DBusCallFlags::NONE,
        1000,
        gio::Cancellable::NONE,
    )
    .map(|_| ())
    .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_player_reader_slows_down_when_nothing_plays() {
        use super::look_again_after;
        use std::time::Duration;
        assert_eq!(look_again_after(true, true), Duration::from_secs(2));
        assert!(look_again_after(false, true) > look_again_after(true, true));
        assert!(look_again_after(false, false) > look_again_after(false, true));
    }

    use super::*;

    fn player(name: &str, playing: bool) -> Player {
        Player {
            bus_name: format!("org.mpris.MediaPlayer2.{name}"),
            name: name.into(),
            title: format!("{name} track"),
            playing,
            can_seek: true,
            volume: 60,
            length_ms: 240_000,
            position_ms: 1000,
            ..Player::default()
        }
    }

    #[test]
    fn the_playerctld_proxy_is_not_a_player() {
        let names: Vec<String> = [
            "org.freedesktop.DBus",
            "org.mpris.MediaPlayer2.spotify",
            "org.mpris.MediaPlayer2.playerctld",
            "org.mpris.MediaPlayer2.DeskLink.phone_4f2a",
            "org.mpris.MediaPlayer2.firefox.instance_1_42",
        ]
        .map(String::from)
        .to_vec();
        assert_eq!(
            player_names(&names),
            [
                "org.mpris.MediaPlayer2.spotify",
                "org.mpris.MediaPlayer2.firefox.instance_1_42"
            ]
        );
    }

    #[test]
    fn a_request_for_a_player_that_is_gone_goes_to_the_active_one() {
        let players = [player("Spotify", false), player("Firefox", true)];
        let none = HashMap::new();
        let name = |found: Option<&Player>| found.map(|player| player.name.clone());
        assert_eq!(
            name(target(&players, Some("Spotify"), &none)),
            Some("Spotify".into())
        );
        assert_eq!(
            name(target(&players, Some("VLC"), &none)),
            Some("Firefox".into()),
            "playing"
        );
        assert_eq!(name(target(&players, None, &none)), Some("Firefox".into()));
        let paused = [player("Spotify", false), player("Firefox", false)];
        let mut last = HashMap::new();
        last.insert("org.mpris.MediaPlayer2.Spotify".to_string(), Instant::now());
        assert_eq!(
            name(target(&paused, Some("VLC"), &last)),
            Some("Spotify".into()),
            "last active"
        );
        assert_eq!(
            name(target(&paused, Some("VLC"), &HashMap::new())),
            Some("Firefox".into())
        );
        assert!(target(&[], Some("VLC"), &none).is_none());
    }

    #[test]
    fn no_players_is_an_empty_list() {
        let packet = state_packet(&[], None);
        assert_eq!(packet.body.get("playerList"), Some(&serde_json::json!([])));
        assert!(!packet.body.contains_key("player"));
    }

    #[test]
    fn the_playing_player_is_the_one_shown_else_the_last_that_played() {
        let players = [player("Spotify", false), player("Firefox", true)];
        let packet = state_packet(&players, active(&players, &HashMap::new()));
        assert_eq!(packet.get_str("player"), Some("Firefox"));
        assert_eq!(packet.get_bool("isPlaying"), Some(true));
        assert_eq!(packet.get_i64("length"), Some(240_000));
        assert_eq!(packet.get_i64("pos"), Some(1000));
        assert_eq!(packet.get_i64("volume"), Some(60));
        assert_eq!(
            packet.body.get("playerList"),
            Some(&serde_json::json!(["Spotify", "Firefox"]))
        );

        let paused = [player("Spotify", false), player("Firefox", false)];
        let mut last = HashMap::new();
        last.insert("org.mpris.MediaPlayer2.Spotify".to_string(), Instant::now());
        assert_eq!(
            active(&paused, &last).map(|p| p.name.as_str()),
            Some("Spotify")
        );
    }

    #[test]
    fn youtube_pages_give_their_thumbnail() {
        let thumb = |id: &str| Some(format!("https://i.ytimg.com/vi/{id}/hqdefault.jpg"));
        for (page, expected) in [
            (
                "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
                thumb("dQw4w9WgXcQ"),
            ),
            (
                "https://youtube.com/watch?list=PL1&v=dQw4w9WgXcQ&t=42",
                thumb("dQw4w9WgXcQ"),
            ),
            (
                "https://music.youtube.com/watch?v=abc-DEF_123",
                thumb("abc-DEF_123"),
            ),
            (
                "https://m.youtube.com/watch?v=dQw4w9WgXcQ#t=1",
                thumb("dQw4w9WgXcQ"),
            ),
            ("https://youtu.be/dQw4w9WgXcQ?si=xyz", thumb("dQw4w9WgXcQ")),
            (
                "https://www.youtube.com/shorts/dQw4w9WgXcQ",
                thumb("dQw4w9WgXcQ"),
            ),
            (
                "http://www.youtube.com/embed/dQw4w9WgXcQ",
                thumb("dQw4w9WgXcQ"),
            ),
            ("https://www.youtube.com/", None),
            ("https://www.youtube.com/watch?v=short", None),
            ("https://www.youtube.com/watch?v=dQw4w9WgXc%22", None),
            ("https://evil.example/watch?v=dQw4w9WgXcQ", None),
            ("https://youtube.com.evil.example/watch?v=dQw4w9WgXcQ", None),
            ("file:///home/me/song.mp3", None),
        ] {
            assert_eq!(youtube_thumbnail(page), expected, "{page}");
        }
    }

    #[test]
    fn unreadable_local_art_is_found_or_falls_back_to_the_video_thumbnail() {
        use std::path::{Path, PathBuf};
        let page = Some("https://www.youtube.com/watch?v=dQw4w9WgXcQ");
        let chrome = Some("file:///tmp/.com.google.Chrome.Ab12Cd".to_string());
        let thumb = Some("https://i.ytimg.com/vi/dQw4w9WgXcQ/hqdefault.jpg".to_string());
        let nowhere = |_: &Path| None;
        // Chrome's Flatpak /tmp, seen from the host.
        let flatpak = |path: &Path| {
            Some(
                PathBuf::from("/run/user/1000/.flatpak/com.google.Chrome/tmp")
                    .join(path.file_name()?),
            )
        };
        assert_eq!(
            usable_art(chrome.clone(), None, flatpak).as_deref(),
            Some("file:///run/user/1000/.flatpak/com.google.Chrome/tmp/.com.google.Chrome.Ab12Cd")
        );
        assert_eq!(
            usable_art(chrome.clone(), page, nowhere),
            thumb,
            "not found anywhere"
        );
        assert_eq!(usable_art(chrome, None, nowhere), None);
        assert_eq!(usable_art(None, page, nowhere), thumb, "no art at all");
        assert_eq!(
            usable_art(None, Some("https://example.com/"), nowhere),
            None
        );
        let remote = Some("https://example.com/a.jpg".to_string());
        assert_eq!(usable_art(remote.clone(), page, nowhere), remote);
        assert_eq!(
            local_art(Path::new("/tmp/../etc/no-such-desklink-file")),
            None
        );
    }

    #[test]
    fn mpris_metadata_maps_to_the_phones_fields() {
        let dict = glib::VariantDict::new(None);
        dict.insert("xesam:title", "Blurred");
        dict.insert(
            "xesam:artist",
            vec!["Kiasmos".to_string(), "Guest".to_string()],
        );
        dict.insert("xesam:album", "Blurred EP");
        dict.insert("mpris:length", 281_000_000i64);
        dict.insert("mpris:artUrl", "https://example.com/a.jpg");
        let (title, artist, album, length_ms, art, _) = metadata(&dict.end());
        assert_eq!(title, "Blurred");
        assert_eq!(artist, "Kiasmos, Guest");
        assert_eq!(album, "Blurred EP");
        assert_eq!(length_ms, 281_000);
        assert_eq!(art.as_deref(), Some("https://example.com/a.jpg"));
        let (title, _, _, length_ms, art, _) = metadata(&glib::VariantDict::new(None).end());
        assert!(title.is_empty() && length_ms == 0 && art.is_none());
    }
}
