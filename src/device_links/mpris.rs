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
    let dict = glib::VariantDict::new(Some(value));
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
    (
        text("xesam:title"),
        artist,
        text("xesam:album"),
        length_us / 1000,
        art,
        track,
    )
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
    glib::VariantDict::new(reply.map(|reply| reply.child_value(0)).as_ref())
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

/// What was last sent, and when a playing track's position was.
#[derive(Default)]
struct Shown {
    key: Option<String>,
    position_sent: Option<Instant>,
    last_playing: HashMap<String, Instant>,
    players: Vec<Player>,
}

static SHOWN: Mutex<Option<Shown>> = Mutex::new(None);

/// Starts the once-a-second read of the local players, once.
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
                loop {
                    poll(&bus);
                    std::thread::sleep(Duration::from_secs(1));
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

/// Sends `device_id` the current state (on connect, or when it asks).
pub(crate) fn send_state_to(device_id: &str) {
    let (packet, art) = {
        let Ok(shown) = SHOWN.lock() else {
            return;
        };
        let Some(shown) = shown.as_ref() else {
            return;
        };
        let current = active(&shown.players, &shown.last_playing);
        (
            state_packet(&shown.players, current),
            current.and_then(current_art),
        )
    };
    send_with_art(device_id, packet, art.as_deref());
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
    fn mpris_metadata_maps_to_the_phones_fields() {
        let dict = glib::VariantDict::new(None);
        dict.insert("xesam:title", "Blurred");
        dict.insert(
            "xesam:artist",
            vec!["Kiasmos".to_string(), "Guest".to_string()],
        );
        dict.insert("xesam:album", "Blurred EP");
        dict.insert("mpris:length", 281_000_000i64);
        dict.insert("mpris:artUrl", "file:///tmp/a.jpg");
        let (title, artist, album, length_ms, art, _) = metadata(&dict.end());
        assert_eq!(title, "Blurred");
        assert_eq!(artist, "Kiasmos, Guest");
        assert_eq!(album, "Blurred EP");
        assert_eq!(length_ms, 281_000);
        assert_eq!(art.as_deref(), Some("file:///tmp/a.jpg"));
        let (title, _, _, length_ms, art, _) = metadata(&glib::VariantDict::new(None).end());
        assert!(title.is_empty() && length_ms == 0 && art.is_none());
    }
}
