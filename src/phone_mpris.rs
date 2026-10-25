//! Each phone's music as an MPRIS player on this desktop, so GNOME's own
//! media controls (the notification list's player) show it and drive it.
//!
//! Every phone with media gets its own session-bus connection that owns
//! `org.mpris.MediaPlayer2.DeskLink.<phone>` and serves
//! `/org/mpris/MediaPlayer2`; one connection each, because two players cannot
//! share an object path on one connection. The state comes from the phone's
//! `desklink.mpris` reports (read once a second); every method becomes a
//! `desklink.mpris.request` to the phone. A phone whose link is down (its
//! report stale) or that has no player is not shown.
//!
//! DeskLink's own players are left out of what the desk sends the phone
//! (`mpris::player_names`), so a phone never sees its own music echoed back.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use gtk::gio;
use gtk::glib::{self, Variant};
use gtk::prelude::*;

use crate::application::DesklinkApplication;
use crate::device_links::daemon::{DaemonCommand, DaemonHandle};
use crate::device_links::device::{DeviceView, MediaStatus};

/// The bus-name prefix of every phone player DeskLink exports.
pub(crate) const BUS_PREFIX: &str = "org.mpris.MediaPlayer2.DeskLink.";
const PATH: &str = "/org/mpris/MediaPlayer2";
const ROOT: &str = "org.mpris.MediaPlayer2";
const PLAYER: &str = "org.mpris.MediaPlayer2.Player";
const TRACK: &str = "/org/desklink/track/current";

const XML: &str = r#"<node>
  <interface name="org.mpris.MediaPlayer2">
    <method name="Raise"/>
    <method name="Quit"/>
    <property name="CanQuit" type="b" access="read"/>
    <property name="CanRaise" type="b" access="read"/>
    <property name="HasTrackList" type="b" access="read"/>
    <property name="Identity" type="s" access="read"/>
    <property name="DesktopEntry" type="s" access="read"/>
    <property name="SupportedUriSchemes" type="as" access="read"/>
    <property name="SupportedMimeTypes" type="as" access="read"/>
  </interface>
  <interface name="org.mpris.MediaPlayer2.Player">
    <method name="Next"/>
    <method name="Previous"/>
    <method name="Pause"/>
    <method name="PlayPause"/>
    <method name="Stop"/>
    <method name="Play"/>
    <method name="Seek"><arg name="Offset" type="x" direction="in"/></method>
    <method name="SetPosition">
      <arg name="TrackId" type="o" direction="in"/>
      <arg name="Position" type="x" direction="in"/>
    </method>
    <method name="OpenUri"><arg name="Uri" type="s" direction="in"/></method>
    <signal name="Seeked"><arg name="Position" type="x"/></signal>
    <property name="PlaybackStatus" type="s" access="read"/>
    <property name="Rate" type="d" access="read"/>
    <property name="Metadata" type="a{sv}" access="read"/>
    <property name="Volume" type="d" access="readwrite"/>
    <property name="Position" type="x" access="read"/>
    <property name="MinimumRate" type="d" access="read"/>
    <property name="MaximumRate" type="d" access="read"/>
    <property name="CanGoNext" type="b" access="read"/>
    <property name="CanGoPrevious" type="b" access="read"/>
    <property name="CanPlay" type="b" access="read"/>
    <property name="CanPause" type="b" access="read"/>
    <property name="CanSeek" type="b" access="read"/>
    <property name="CanControl" type="b" access="read"/>
  </interface>
</node>"#;

/// `org.mpris.MediaPlayer2.DeskLink.<phone>`: a valid bus-name element made
/// from the device id (letters, digits and `_`, never a leading digit).
pub(crate) fn bus_name_for(device_id: &str) -> String {
    let element: String = device_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .take(64)
        .collect();
    format!("{BUS_PREFIX}phone_{element}")
}

/// What one exported player shows.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Shown {
    pub(crate) identity: String,
    pub(crate) player: String,
    pub(crate) playing: bool,
    pub(crate) title: String,
    pub(crate) artist: String,
    pub(crate) album: String,
    /// Microseconds.
    pub(crate) length_us: i64,
    pub(crate) position_us: i64,
    pub(crate) volume: f64,
    pub(crate) can_seek: bool,
    pub(crate) art_url: Option<String>,
}

/// The player to show for `device`, if any: its media is live (the link is
/// up) and it has one.
pub(crate) fn shown_for(device: &DeviceView) -> Option<Shown> {
    if !device.trusted || !device.feature_transport_ready {
        return None;
    }
    let media: &MediaStatus = device.media_status.as_ref()?;
    if media.player.is_empty() && media.player_list.is_empty() {
        return None;
    }
    let player = if media.player.is_empty() {
        media.player_list.first().cloned().unwrap_or_default()
    } else {
        media.player.clone()
    };
    Some(Shown {
        identity: format!("{player} on {}", device.name),
        player,
        playing: media.is_playing,
        title: media.title.clone(),
        artist: media.artist.clone(),
        album: media.album.clone(),
        length_us: media.length.unwrap_or(0).saturating_mul(1000),
        position_us: media.position.unwrap_or(0).saturating_mul(1000),
        volume: media
            .volume
            .map_or(1.0, |volume| (volume as f64 / 100.0).clamp(0.0, 1.0)),
        can_seek: media.can_seek,
        // The cover the phone sent; else a web address (a phone-side path
        // means nothing here).
        art_url: media
            .album_art
            .as_ref()
            .and_then(|art| art.file.as_ref())
            .and_then(|file| glib::filename_to_uri(file, None).ok())
            .map(String::from)
            .or_else(|| {
                media
                    .album_art_url
                    .clone()
                    .filter(|url| url.starts_with("https://") || url.starts_with("http://"))
            }),
    })
}

/// MPRIS `Metadata` for `shown`.
pub(crate) fn metadata(shown: &Shown) -> Variant {
    let dict = glib::VariantDict::new(None);
    if let Ok(track) = glib::variant::ObjectPath::try_from(TRACK) {
        dict.insert_value("mpris:trackid", &track.to_variant());
    }
    dict.insert("xesam:title", &shown.title);
    if !shown.artist.is_empty() {
        dict.insert("xesam:artist", vec![shown.artist.clone()]);
    }
    if !shown.album.is_empty() {
        dict.insert("xesam:album", &shown.album);
    }
    if shown.length_us > 0 {
        dict.insert("mpris:length", shown.length_us);
    }
    if let Some(url) = &shown.art_url {
        dict.insert("mpris:artUrl", url);
    }
    dict.end()
}

fn player_property(shown: &Shown, name: &str) -> Option<Variant> {
    Some(match name {
        "PlaybackStatus" => if shown.playing { "Playing" } else { "Paused" }.to_variant(),
        "Rate" | "MinimumRate" | "MaximumRate" => 1.0f64.to_variant(),
        "Metadata" => metadata(shown),
        "Volume" => shown.volume.to_variant(),
        "Position" => shown.position_us.to_variant(),
        "CanSeek" => shown.can_seek.to_variant(),
        "CanGoNext" | "CanGoPrevious" | "CanPlay" | "CanPause" | "CanControl" => true.to_variant(),
        _ => return None,
    })
}

fn root_property(shown: &Shown, name: &str) -> Option<Variant> {
    Some(match name {
        "CanQuit" | "HasTrackList" => false.to_variant(),
        "CanRaise" => true.to_variant(),
        "Identity" => shown.identity.to_variant(),
        "DesktopEntry" => "derx06.desklink.com".to_variant(),
        "SupportedUriSchemes" | "SupportedMimeTypes" => Vec::<String>::new().to_variant(),
        _ => return None,
    })
}

/// The phone request a player method makes; `None` for what DeskLink does
/// itself (Raise) or ignores.
pub(crate) fn request_for(
    device_id: &str,
    shown: &Shown,
    method: &str,
    params: &Variant,
) -> Option<DaemonCommand> {
    let (id, player) = (device_id.to_string(), shown.player.clone());
    Some(match method {
        "Play" | "Pause" | "PlayPause" | "Next" | "Previous" | "Stop" => {
            DaemonCommand::SendMprisAction(id, player, method.to_string())
        }
        "Seek" => DaemonCommand::SendMprisSeek(id, player, params.get::<(i64,)>()?.0),
        "SetPosition" => {
            let position_us = params.try_child_value(1)?.get::<i64>()?;
            DaemonCommand::SendMprisSetPosition(id, player, (position_us / 1000).max(0))
        }
        _ => return None,
    })
}

/// One phone's exported player.
struct Export {
    connection: gio::DBusConnection,
    owner: gio::OwnerId,
    shown: Rc<RefCell<Shown>>,
}

impl Export {
    fn start(
        app: &DesklinkApplication,
        daemon: DaemonHandle,
        device_id: &str,
        shown: Shown,
    ) -> Result<Self, String> {
        let address =
            gio::dbus_address_get_for_bus_sync(gio::BusType::Session, gio::Cancellable::NONE)
                .map_err(|error| error.to_string())?;
        let connection = gio::DBusConnection::for_address_sync(
            &address,
            gio::DBusConnectionFlags::AUTHENTICATION_CLIENT
                | gio::DBusConnectionFlags::MESSAGE_BUS_CONNECTION,
            None,
            gio::Cancellable::NONE,
        )
        .map_err(|error| error.to_string())?;
        let node = gio::DBusNodeInfo::for_xml(XML).map_err(|error| error.to_string())?;
        let shown = Rc::new(RefCell::new(shown));
        for interface in [ROOT, PLAYER] {
            let info = node
                .lookup_interface(interface)
                .ok_or("bad MPRIS description")?;
            let (state, daemon, id, app) = (
                Rc::clone(&shown),
                daemon.clone(),
                device_id.to_string(),
                app.downgrade(),
            );
            let read = Rc::clone(&shown);
            let write = (daemon.clone(), device_id.to_string(), Rc::clone(&shown));
            connection
                .register_object(PATH, &info)
                .method_call(move |_, _, _, _, method, params, invocation| {
                    crate::guard("phone media call", (), || {
                        if method == "Raise" {
                            if let Some(app) = app.upgrade() {
                                app.activate();
                            }
                        } else if let Some(command) =
                            request_for(&id, &state.borrow(), method, &params)
                        {
                            daemon.send(command);
                        }
                        invocation.return_value(None);
                    })
                })
                .property(move |_, _, _, interface, name| {
                    let shown = read.borrow();
                    let value = if interface == ROOT {
                        root_property(&shown, name)
                    } else {
                        player_property(&shown, name)
                    };
                    value.unwrap_or_else(|| false.to_variant())
                })
                .set_property(move |_, _, _, _, name, value| {
                    let (daemon, id, shown) = &write;
                    if name != "Volume" {
                        return false;
                    }
                    let Some(level) = value.get::<f64>() else {
                        return false;
                    };
                    let player = shown.borrow().player.clone();
                    daemon.send(DaemonCommand::SendMprisSetVolume(
                        id.clone(),
                        player,
                        (level.clamp(0.0, 1.0) * 100.0).round() as i64,
                    ));
                    true
                })
                .build()
                .map_err(|error| error.to_string())?;
        }
        let owner = gio::bus_own_name_on_connection(
            &connection,
            &bus_name_for(device_id),
            gio::BusNameOwnerFlags::NONE,
            |_, _| {},
            |_, name| eprintln!("[DeskLink] Could not own {name}"),
        );
        Ok(Self {
            connection,
            owner,
            shown,
        })
    }

    /// Shows `next`, telling listeners what changed.
    fn update(&self, next: Shown) {
        let changed: HashMap<String, Variant> = [
            "PlaybackStatus",
            "Metadata",
            "Volume",
            "Position",
            "CanSeek",
        ]
        .into_iter()
        .filter_map(|name| {
            let now = player_property(&next, name)?;
            (player_property(&self.shown.borrow(), name).as_ref() != Some(&now))
                .then(|| (name.to_string(), now))
        })
        .collect();
        let identity_changed = self.shown.borrow().identity != next.identity;
        self.shown.replace(next);
        let emit = |interface: &str, changed: HashMap<String, Variant>| {
            let _ = self.connection.emit_signal(
                None,
                PATH,
                "org.freedesktop.DBus.Properties",
                "PropertiesChanged",
                Some(&(interface, changed, Vec::<String>::new()).to_variant()),
            );
        };
        if !changed.is_empty() {
            emit(PLAYER, changed);
        }
        if identity_changed {
            let identity = root_property(&self.shown.borrow(), "Identity");
            emit(
                ROOT,
                identity
                    .map(|identity| HashMap::from([("Identity".to_string(), identity)]))
                    .unwrap_or_default(),
            );
        }
    }

    fn stop(self) {
        gio::bus_unown_name(self.owner);
        let _ = self.connection.close_sync(gio::Cancellable::NONE);
    }
}

/// Starts keeping the exported players in step with the phones, once a
/// second, for as long as `app` runs.
pub fn start(app: &DesklinkApplication) {
    let exports: Rc<RefCell<HashMap<String, Export>>> = Rc::default();
    let weak = app.downgrade();
    crate::ticker::every_tick(move || {
        let Some(app) = weak.upgrade().filter(|app| !app.is_remote()) else {
            for (_, export) in exports.borrow_mut().drain() {
                export.stop();
            }
            return glib::ControlFlow::Break;
        };
        let Some(daemon) = app.daemon_handle() else {
            return glib::ControlFlow::Continue;
        };
        let wanted: HashMap<String, Shown> = daemon
            .devices()
            .iter()
            .filter_map(|device| Some((device.id.clone(), shown_for(device)?)))
            .collect();
        let mut exports = exports.borrow_mut();
        let gone: Vec<String> = exports
            .keys()
            .filter(|id| !wanted.contains_key(*id))
            .cloned()
            .collect();
        for id in gone {
            if let Some(export) = exports.remove(&id) {
                export.stop();
            }
        }
        for (id, shown) in wanted {
            match exports.get(&id) {
                Some(export) => export.update(shown),
                None => match Export::start(&app, daemon.clone(), &id, shown) {
                    Ok(export) => {
                        exports.insert(id, export);
                    }
                    Err(error) => eprintln!(
                        "[DeskLink] Phone media not shown in the desktop media controls: {error}"
                    ),
                },
            }
        }
        glib::ControlFlow::Continue
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device_links::device_info::DeviceInfo;

    fn phone(media: Option<MediaStatus>, ready: bool) -> DeviceView {
        let info = DeviceInfo {
            id: "4f2a-90".into(),
            name: "realme 14 5G".into(),
            device_type: "phone".into(),
            protocol_version: 9,
            incoming_capabilities: Vec::new(),
            outgoing_capabilities: Vec::new(),
            instance_id: None,
            app_version: None,
        };
        let mut device = DeviceView::from_info(&info, "10.0.0.2".into(), true);
        device.feature_transport_ready = ready;
        device.media_status = media;
        device
    }

    fn media() -> MediaStatus {
        MediaStatus {
            player: "Spotify".into(),
            player_list: vec!["Spotify".into()],
            title: "Blurred".into(),
            artist: "Kiasmos".into(),
            album: "Blurred EP".into(),
            is_playing: true,
            volume: Some(40),
            position: Some(12_000),
            length: Some(281_000),
            can_seek: true,
            album_art_url: Some("content://media/42".into()),
            album_art: None,
        }
    }

    #[test]
    fn bus_names_are_valid_whatever_the_device_id() {
        assert_eq!(
            bus_name_for("4f2a-90"),
            "org.mpris.MediaPlayer2.DeskLink.phone_4f2a_90"
        );
        assert_eq!(
            bus_name_for("0123"),
            "org.mpris.MediaPlayer2.DeskLink.phone_0123"
        );
        let odd = bus_name_for("a.b/c d😀");
        assert!(odd
            .trim_start_matches(BUS_PREFIX)
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_'));
        assert!(gio::dbus_is_name(&odd));
    }

    #[test]
    fn a_phone_player_maps_to_mpris_and_hides_when_stale() {
        let shown = shown_for(&phone(Some(media()), true)).unwrap();
        assert_eq!(shown.identity, "Spotify on realme 14 5G");
        assert_eq!(shown.length_us, 281_000_000);
        assert_eq!(shown.position_us, 12_000_000);
        assert!((shown.volume - 0.4).abs() < 1e-9);
        assert_eq!(shown.art_url, None, "a phone-side URL means nothing here");
        assert_eq!(
            player_property(&shown, "PlaybackStatus")
                .unwrap()
                .get::<String>()
                .unwrap(),
            "Playing"
        );
        let meta = glib::VariantDict::new(Some(&metadata(&shown)));
        assert_eq!(
            meta.lookup::<String>("xesam:title").unwrap().unwrap(),
            "Blurred"
        );
        assert_eq!(
            meta.lookup::<Vec<String>>("xesam:artist").unwrap().unwrap(),
            ["Kiasmos"]
        );
        assert_eq!(
            meta.lookup::<i64>("mpris:length").unwrap().unwrap(),
            281_000_000
        );

        assert!(shown_for(&phone(Some(media()), false)).is_none(), "stale");
        assert!(shown_for(&phone(None, true)).is_none(), "no media");
    }

    #[test]
    fn player_methods_become_requests_to_the_phone() {
        let shown = shown_for(&phone(Some(media()), true)).unwrap();
        let unit = ().to_variant();
        assert!(matches!(
            request_for("p", &shown, "PlayPause", &unit),
            Some(DaemonCommand::SendMprisAction(_, player, action)) if player == "Spotify" && action == "PlayPause"
        ));
        assert!(matches!(
            request_for("p", &shown, "Seek", &(5_000_000i64,).to_variant()),
            Some(DaemonCommand::SendMprisSeek(_, _, 5_000_000))
        ));
        let track = glib::variant::ObjectPath::try_from(TRACK).unwrap();
        assert!(matches!(
            request_for(
                "p",
                &shown,
                "SetPosition",
                &(track, 90_000_000i64).to_variant()
            ),
            Some(DaemonCommand::SendMprisSetPosition(_, _, 90_000))
        ));
        assert!(request_for("p", &shown, "OpenUri", &("x",).to_variant()).is_none());
    }
}
