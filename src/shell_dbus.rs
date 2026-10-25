//! `derx06.desklink.com.Shell1` at `/derx06/desklink/com/Shell`: what the
//! GNOME Shell extension (`gnome-shell-extension/`) and any other top-bar
//! client read and do.
//!
//! Exported on the application's own connection, so it lives exactly as long
//! as the `derx06.desklink.com` name. `Devices` is polled from the daemon once
//! a second and `PropertiesChanged` is emitted only when it changed. `Invoke`
//! carries no data in: files to send are picked in DeskLink's own dialog,
//! never accepted as paths over the bus.
//!
//! Also offers that extension, once, on GNOME without a tray host.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::gio;
use gtk::glib::variant::ToVariant;
use gtk::glib::{self, Variant};

use crate::application::DesklinkApplication;
use crate::device_links::config::AppSettings;
use crate::device_links::daemon::DaemonCommand;
use crate::device_links::device::{DesktopSession, DeviceStatus, DeviceView};

pub const PATH: &str = "/derx06/desklink/com/Shell";
pub const INTERFACE: &str = "derx06.desklink.com.Shell1";
/// 2: `Desk`, and the `unlock-via-phone`, `lock-hand-off`, `keep-awake-*`
/// and `clipboard-*` actions.
/// 3: `Notifications`, `Transfers`, `Desk.pill`, `Devices[].lastSeen`, the
/// `lock`, `send-clipboard` and `reconnect` actions, and the notification and
/// transfer methods.
/// 4: `Devices[].call` and the `call-*` actions (a client that shows calls
/// says `calls` in `Hello`).
/// 5: `Devices[].media` with the `media-play-pause` / `media-next` actions,
/// and `Devices[].liveSince`.
/// 6: `Snippets` (id, label, text: the client copies the text itself) and
/// the device-less `new-note` action.
/// 7: `Devices[].bucket` (`line`, `detail`, `fraction`) while the Bucket
/// syncs, and the `open-bucket` action.
const VERSION: u32 = 7;
const INVALID_ARGS: &str = "org.freedesktop.DBus.Error.InvalidArgs";

const XML: &str = r#"<node>
  <interface name="derx06.desklink.com.Shell1">
    <property name="Version" type="u" access="read"/>
    <property name="Devices" type="aa{sv}" access="read"/>
    <property name="Desk" type="a{sv}" access="read"/>
    <property name="Notifications" type="aa{sv}" access="read"/>
    <property name="Transfers" type="aa{sv}" access="read"/>
    <property name="Snippets" type="aa{sv}" access="read"/>
    <method name="Invoke">
      <arg name="device_id" type="s" direction="in"/>
      <arg name="action" type="s" direction="in"/>
    </method>
    <method name="NotificationAction">
      <arg name="device_id" type="s" direction="in"/>
      <arg name="notification_id" type="s" direction="in"/>
      <arg name="action" type="s" direction="in"/>
    </method>
    <method name="NotificationReply">
      <arg name="device_id" type="s" direction="in"/>
      <arg name="notification_id" type="s" direction="in"/>
      <arg name="text" type="s" direction="in"/>
    </method>
    <method name="NotificationDismiss">
      <arg name="device_id" type="s" direction="in"/>
      <arg name="notification_id" type="s" direction="in"/>
    </method>
    <method name="OpenTransfer">
      <arg name="transfer_id" type="s" direction="in"/>
    </method>
    <method name="Hello">
      <arg name="version" type="u" direction="in"/>
      <arg name="capabilities" type="as" direction="in"/>
    </method>
  </interface>
</node>"#;

/// Actions that need no device (`device_id` ""): `end-sessions` then means
/// every device.
const DEVICELESS: [&str; 10] = [
    "open",
    "new-note",
    "reconnect",
    "pair-new",
    "end-sessions",
    "lock-hand-off",
    "keep-awake-on",
    "keep-awake-off",
    "clipboard-on",
    "clipboard-off",
];

/// This computer's switches, for the menu's tiles.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Desk {
    keep_awake: bool,
    clipboard: bool,
    /// A terminal is running and a phone is connected.
    hand_off: bool,
    /// Settings → Appearance → "Phone in the top bar": the extension's own pill, on
    /// top of the Quick Settings menu.
    pill: bool,
}

impl Desk {
    fn to_variant(&self) -> Variant {
        let dict = glib::VariantDict::new(None);
        dict.insert("keepAwake", self.keep_awake);
        dict.insert("clipboard", self.clipboard);
        dict.insert("handOff", self.hand_off);
        dict.insert("pill", self.pill);
        dict.end()
    }
}

fn desk(devices: &[ShellDevice]) -> Desk {
    let settings = crate::device_links::config::app_settings();
    Desk {
        keep_awake: settings.stay_awake,
        clipboard: settings.clipboard_sync,
        hand_off: devices.iter().any(|device| device.ready)
            && crate::terminal_previews::sessions()
                .iter()
                .any(|info| info.exit_status.is_none()),
        pill: settings.top_bar_pill,
    }
}

/// One device as clients see it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ShellDevice {
    id: String,
    name: String,
    kind: String,
    paired: bool,
    reachable: bool,
    ready: bool,
    battery: i32,
    charging: bool,
    path: String,
    screen: &'static str,
    terminals: u32,
    /// When its link was lost (Unix seconds), while it stays lost; 0 live.
    last_seen: u64,
    /// Its call now, live links only.
    call: Option<ShellCall>,
    /// The phone's music now, live links only.
    media: Option<ShellMedia>,
    /// Its Bucket sync, while one is going.
    bucket: Option<ShellBucket>,
    /// Since when it has been using this computer (Unix seconds); 0 not.
    live_since: u64,
    actions: Vec<&'static str>,
}

/// The phone's Bucket sync now: `Devices[].bucket`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ShellBucket {
    /// "Syncing 3 files · 1.1 GB of 2.4 GB".
    line: String,
    /// "12 MB/s · about 4 min left".
    detail: String,
    /// 0..=1000.
    permille: u32,
}

fn shell_bucket(summary: &crate::bucket_progress::Summary) -> ShellBucket {
    ShellBucket {
        line: summary.head(),
        detail: summary.rate_line(),
        permille: (summary.fraction() * 1000.0).round() as u32,
    }
}

/// The phone's music: `Devices[].media`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ShellMedia {
    title: String,
    artist: String,
    playing: bool,
}

fn shell_media(device: &DeviceView) -> Option<ShellMedia> {
    let media = device
        .media_status
        .as_ref()
        .filter(|media| device.feature_transport_ready && !media.title.is_empty())?;
    Some(ShellMedia {
        title: media.title.clone(),
        artist: media.artist.clone(),
        playing: media.is_playing,
    })
}

thread_local! {
    /// When each device started using this computer, while it does.
    static LIVE_SINCE: RefCell<std::collections::HashMap<String, u64>> = RefCell::default();
}

/// Fills `live_since`: the first time each device was seen live, kept while
/// it stays live.
fn stamp_live(devices: &mut [ShellDevice]) {
    let now = crate::device_links::connection_log::now();
    LIVE_SINCE.with(|since| {
        let mut since = since.borrow_mut();
        since.retain(|id, _| {
            devices
                .iter()
                .any(|device| &device.id == id && is_live(device))
        });
        for device in devices.iter_mut().filter(|device| is_live(device)) {
            device.live_since = *since.entry(device.id.clone()).or_insert(now);
        }
    });
}

fn is_live(device: &ShellDevice) -> bool {
    !device.screen.is_empty() || device.terminals > 0
}

/// The call a client shows: `Devices[].call`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ShellCall {
    /// The phone's id for it, the same for all its updates.
    id: String,
    /// "ringing", "dialing", "talking" or "held".
    state: &'static str,
    incoming: bool,
    /// The contact, else the number, else "Incoming call" (the phone keeps
    /// the caller private unless its user allowed it).
    caller: String,
    /// "" when not sent.
    number: String,
    /// Unix milliseconds; 0 unknown.
    started_at: i64,
    muted: bool,
    silenced: bool,
}

fn shell_call(device: &DeviceView) -> Option<ShellCall> {
    use crate::device_links::device::CallState;
    let status = device
        .call_status
        .as_ref()
        .filter(|_| device.feature_transport_ready)?;
    let call = status.primary();
    Some(ShellCall {
        id: call.call_id.clone(),
        state: match call.state {
            CallState::Ringing => "ringing",
            CallState::Dialing => "dialing",
            CallState::Talking => "talking",
            CallState::Held => "held",
        },
        incoming: call.incoming,
        caller: call.caller(),
        number: call.number.clone().unwrap_or_default(),
        started_at: call.started_at.unwrap_or(0),
        muted: status.muted,
        silenced: status.silenced,
    })
}

fn shell_device(device: &DeviceView, settings: &AppSettings) -> ShellDevice {
    let battery = device.battery_status.as_ref();
    ShellDevice {
        id: device.id.clone(),
        name: device.name.clone(),
        kind: device.device_type.clone(),
        paired: device.trusted,
        reachable: !matches!(
            device.status,
            DeviceStatus::Discovered | DeviceStatus::Unreachable
        ),
        ready: device.feature_transport_ready,
        // Only a live reading: a stale one would read as current in the top bar.
        battery: battery
            .filter(|_| device.feature_transport_ready)
            .map_or(-1, |battery| battery.current_charge.clamp(0, 100) as i32),
        charging: device.feature_transport_ready
            && battery.is_some_and(|battery| battery.is_charging),
        path: device.connection_path.clone().unwrap_or_default(),
        screen: match device.desktop_session {
            // Glide (F4): this computer's pointer is on the phone.
            DesktopSession::None
                if crate::device_links::glide::status().gliding.as_deref()
                    == Some(device.id.as_str()) =>
            {
                "glide"
            }
            DesktopSession::None => "",
            DesktopSession::Viewing => "view",
            DesktopSession::Controlling => "control",
        },
        terminals: device.terminal_sessions_attached as u32,
        last_seen: device.last_seen.unwrap_or(0),
        call: shell_call(device),
        media: shell_media(device),
        bucket: crate::bucket_progress::summary(&device.id).map(|summary| shell_bucket(&summary)),
        live_since: 0,
        actions: actions(device, settings),
    }
}

/// What a client may offer for `device` right now.
fn actions(device: &DeviceView, settings: &AppSettings) -> Vec<&'static str> {
    let mut actions = vec!["open"];
    if device.feature_transport_ready {
        actions.extend([
            "ring",
            "lock",
            "send-files",
            "send-clipboard",
            "new-terminal",
        ]);
    }
    // The lock-screen button: only with "Unlock with … nearby" on and the
    // phone connected on this network.
    if crate::device_links::desk_features::could_unlock(device, settings) {
        actions.push("unlock-via-phone");
    }
    // The call's buttons, when the phone takes call requests.
    if let Some(status) = device.call_status.as_ref().filter(|_| {
        device.feature_transport_ready
            && device.incoming_capabilities.iter().any(|capability| {
                capability == crate::device_links::packet::PACKET_TYPE_TELEPHONY_REQUEST
            })
    }) {
        for (action, ..) in crate::window::calls::buttons(status) {
            actions.push(match action {
                "answer" => "call-answer",
                "decline" => "call-decline",
                "silence" => "call-silence",
                "mute" => "call-mute",
                "unmute" => "call-unmute",
                _ => "call-end",
            });
        }
    }
    if shell_media(device).is_some() {
        actions.extend(["media-play-pause", "media-next"]);
    }
    if device.trusted && crate::device_links::folders::bucket_on(&device.id) {
        actions.push("open-bucket");
    }
    // "Can't reach … · Try again" has something to press.
    if device
        .last_error
        .as_deref()
        .is_some_and(crate::device_links::webrtc::coordinator::is_unavailable)
    {
        actions.push("try-again");
    }
    // Ending is local (stop sharing, detach terminals), so it does not wait
    // for the feature transport.
    if device.desktop_session != DesktopSession::None || device.terminal_sessions_attached > 0 {
        actions.push("end-sessions");
    }
    actions
}

impl ShellDevice {
    fn to_dict(&self) -> Variant {
        let dict = glib::VariantDict::new(None);
        dict.insert("id", &self.id);
        dict.insert("name", &self.name);
        dict.insert("type", &self.kind);
        dict.insert("paired", self.paired);
        dict.insert("reachable", self.reachable);
        dict.insert("ready", self.ready);
        dict.insert("battery", self.battery);
        dict.insert("charging", self.charging);
        dict.insert("path", &self.path);
        dict.insert("screen", self.screen);
        dict.insert("terminals", self.terminals);
        dict.insert("lastSeen", self.last_seen);
        if let Some(call) = &self.call {
            let entry = glib::VariantDict::new(None);
            entry.insert("id", &call.id);
            entry.insert("state", call.state);
            entry.insert("incoming", call.incoming);
            entry.insert("caller", &call.caller);
            entry.insert("number", &call.number);
            entry.insert("startedAt", call.started_at);
            entry.insert("muted", call.muted);
            entry.insert("silenced", call.silenced);
            dict.insert_value("call", &entry.end());
        }
        if let Some(media) = &self.media {
            let entry = glib::VariantDict::new(None);
            entry.insert("title", &media.title);
            entry.insert("artist", &media.artist);
            entry.insert("playing", media.playing);
            dict.insert_value("media", &entry.end());
        }
        if let Some(bucket) = &self.bucket {
            let entry = glib::VariantDict::new(None);
            entry.insert("line", &bucket.line);
            entry.insert("detail", &bucket.detail);
            entry.insert("fraction", f64::from(bucket.permille) / 1000.0);
            dict.insert_value("bucket", &entry.end());
        }
        dict.insert("liveSince", self.live_since);
        dict.insert("actions", &self.actions);
        dict.end()
    }
}

/// The phones' notifications, for the extension's message tray: only phones
/// whose notifications are shown here. `silent` ones were resent on a
/// reconnect and never banner; `ready` false means they are stale (no
/// actions).
fn notifications_variant(devices: &[DeviceView], settings: &AppSettings) -> Variant {
    let dicts = devices
        .iter()
        .filter(|device| device.trusted && settings.shows_notifications(&device.id))
        .flat_map(|device| {
            device.notifications.iter().map(move |notification| {
                let dict = glib::VariantDict::new(None);
                dict.insert("device", &device.id);
                dict.insert("deviceName", &device.name);
                dict.insert("id", &notification.id);
                dict.insert("app", &notification.app_name);
                dict.insert(
                    "icon",
                    crate::window::notifications::app_icon(&notification.app_name),
                );
                dict.insert("title", &notification.title);
                dict.insert(
                    "body",
                    if notification.text.is_empty() {
                        &notification.ticker
                    } else {
                        &notification.text
                    },
                );
                dict.insert("actions", &notification.actions);
                dict.insert("replyable", notification.request_reply_id.is_some());
                dict.insert("dismissable", notification.is_clearable);
                dict.insert("silent", notification.silent);
                dict.insert("ready", device.feature_transport_ready);
                dict.end()
            })
        });
    Variant::array_from_iter_with_type(glib::VariantTy::VARDICT, dicts)
}

/// How many recent files the menu lists.
const RECENT_TRANSFERS: usize = 5;

/// The newest files sent or received; `id` is the path, which only
/// `OpenTransfer` reads back (and only for a file still on this list).
fn transfers_variant(files: &[crate::device_links::transfer_log::LoggedFile]) -> Variant {
    let dicts = files.iter().take(RECENT_TRANSFERS).map(|file| {
        let dict = glib::VariantDict::new(None);
        dict.insert("id", file.path.to_string_lossy().as_ref());
        dict.insert("name", &file.name);
        dict.insert("deviceName", &file.device_name);
        dict.insert("incoming", file.incoming);
        dict.insert("time", file.ts);
        dict.end()
    });
    Variant::array_from_iter_with_type(glib::VariantTy::VARDICT, dicts)
}

/// The snippets the menu shows (F6), newest first.
fn snippets_variant() -> Variant {
    let dicts = crate::device_links::notes::snippets()
        .into_iter()
        .take(RECENT_SNIPPETS)
        .map(|snippet| {
            let dict = glib::VariantDict::new(None);
            dict.insert("id", &snippet.id);
            dict.insert("label", snippet.label());
            dict.insert("text", &snippet.text);
            dict.end()
        });
    Variant::array_from_iter_with_type(glib::VariantTy::VARDICT, dicts)
}

/// How many snippets the top-bar menu gets.
const RECENT_SNIPPETS: usize = 8;

fn devices_variant(devices: &[ShellDevice]) -> Variant {
    Variant::array_from_iter_with_type(
        glib::VariantTy::VARDICT,
        devices.iter().map(ShellDevice::to_dict),
    )
}

/// The device (if any) `action` targets, once it is allowed now.
fn check<'a>(
    devices: &'a [ShellDevice],
    device_id: &str,
    action: &str,
) -> Result<Option<&'a ShellDevice>, String> {
    if device_id.is_empty() {
        return if DEVICELESS.contains(&action) {
            Ok(None)
        } else {
            Err(format!("\"{action}\" needs a device"))
        };
    }
    let device = devices
        .iter()
        .find(|device| device.id == device_id)
        .ok_or_else(|| format!("no device \"{device_id}\""))?;
    if device.actions.contains(&action) {
        Ok(Some(device))
    } else {
        Err(format!("\"{action}\" is not available for this device now"))
    }
}

fn current(app: &DesklinkApplication) -> Vec<ShellDevice> {
    let settings = crate::device_links::config::app_settings();
    app.daemon_handle()
        .map(|daemon| {
            daemon
                .devices()
                .iter()
                .map(|device| shell_device(device, &settings))
                .collect()
        })
        .unwrap_or_default()
}

/// Exports the interface on `connection` (from `dbus_register`, so it is
/// there before the application's name is) and starts the poll.
pub fn export(app: &DesklinkApplication, connection: &gio::DBusConnection) {
    let interface = gio::DBusNodeInfo::for_xml(XML)
        .ok()
        .and_then(|node| node.lookup_interface(INTERFACE));
    let Some(interface) = interface else {
        eprintln!("[Shell] Invalid interface description");
        return;
    };
    let properties = Rc::new(RefCell::new(property_values(app)));

    let weak = app.downgrade();
    let shown = Rc::clone(&properties);
    let registered = connection
        .register_object(PATH, &interface)
        .method_call(
            move |connection, sender, _, _, method, params, invocation| {
                crate::guard("top-bar call", (), || {
                    let Some(app) = weak.upgrade() else {
                        return invocation.return_dbus_error(INVALID_ARGS, "DeskLink is quitting");
                    };
                    if method == "Hello" {
                        let (_, capabilities) =
                            params.get::<(u32, Vec<String>)>().unwrap_or_default();
                        let sender = sender.unwrap_or_default();
                        // Only the Shell itself (the extension runs inside it)
                        // may say it shows the alerts: anyone else claiming so
                        // would mute DeskLink's own call and notification alerts.
                        if hello_is_from_the_shell(&connection, sender) {
                            client_said_hello(&connection, sender, vetted(capabilities));
                        } else {
                            static TOLD: std::sync::Once = std::sync::Once::new();
                            TOLD.call_once(|| {
                                eprintln!(
                                    "[Shell] Ignored a Hello from a client that is not GNOME Shell"
                                );
                            });
                        }
                        return invocation.return_value(None);
                    }
                    match call(&app, method, &params) {
                        Ok(()) => invocation.return_value(None),
                        Err(error) => invocation.return_dbus_error(INVALID_ARGS, &error),
                    }
                })
            },
        )
        .property(move |_, _, _, _, name| {
            shown
                .borrow()
                .get(name)
                .cloned()
                .unwrap_or_else(|| VERSION.to_variant())
        })
        .build();
    if let Err(error) = registered {
        eprintln!("[Shell] Could not export {PATH}: {error}");
        return;
    }

    track_extension(connection);

    let weak = app.downgrade();
    let connection = connection.clone();
    crate::ticker::every_tick(move || {
        crate::guard("top-bar tick", glib::ControlFlow::Continue, || {
            let Some(app) = weak.upgrade().filter(|app| !app.is_remote()) else {
                return glib::ControlFlow::Break;
            };
            let now = property_values(&app);
            let changed: std::collections::HashMap<String, Variant> = now
                .iter()
                .filter(|(name, value)| properties.borrow().get(*name) != Some(*value))
                .map(|(name, value)| (name.to_string(), value.clone()))
                .collect();
            properties.replace(now);
            if !changed.is_empty() {
                let changed = (INTERFACE, changed, Vec::<String>::new()).to_variant();
                let _ = connection.emit_signal(
                    None,
                    PATH,
                    "org.freedesktop.DBus.Properties",
                    "PropertiesChanged",
                    Some(&changed),
                );
            }
            glib::ControlFlow::Continue
        })
    });

    offer_extension(app);
}

/// Every property's current value, by name.
fn property_values(app: &DesklinkApplication) -> std::collections::HashMap<&'static str, Variant> {
    let settings = crate::device_links::config::app_settings();
    let views = app
        .daemon_handle()
        .map(|daemon| daemon.devices())
        .unwrap_or_default();
    let mut devices: Vec<ShellDevice> = views
        .iter()
        .map(|device| shell_device(device, &settings))
        .collect();
    stamp_live(&mut devices);
    std::collections::HashMap::from([
        ("Version", VERSION.to_variant()),
        ("Desk", desk(&devices).to_variant()),
        ("Devices", devices_variant(&devices)),
        ("Notifications", notifications_variant(&views, &settings)),
        (
            "Transfers",
            transfers_variant(&crate::device_links::transfer_log::recent()),
        ),
        ("Snippets", snippets_variant()),
    ])
}

/// One method call from a client.
fn call(app: &DesklinkApplication, method: &str, params: &Variant) -> Result<(), String> {
    let daemon = app.daemon_handle().ok_or("DeskLink is starting")?;
    match method {
        "Invoke" => {
            let (device_id, action) = params.get::<(String, String)>().unwrap_or_default();
            let devices = current(app);
            let device = check(&devices, &device_id, &action)?;
            perform(app, device, &action);
            Ok(())
        }
        "NotificationAction" | "NotificationReply" | "NotificationDismiss" => {
            let (device_id, notification_id, argument) = match method {
                "NotificationDismiss" => {
                    let (device, id) = params.get::<(String, String)>().unwrap_or_default();
                    (device, id, String::new())
                }
                _ => params.get::<(String, String, String)>().unwrap_or_default(),
            };
            let device = daemon
                .devices()
                .into_iter()
                .find(|device| device.id == device_id && device.trusted)
                .ok_or("no such phone")?;
            if !device.feature_transport_ready {
                return Err(format!("{} is not connected", device.name));
            }
            let notification = device
                .notifications
                .iter()
                .find(|notification| notification.id == notification_id)
                .ok_or("no such notification")?;
            match method {
                "NotificationAction" if notification.actions.contains(&argument) => daemon.send(
                    DaemonCommand::TriggerNotificationAction(device_id, notification_id, argument),
                ),
                "NotificationReply" if !argument.trim().is_empty() => {
                    let reply_id = notification
                        .request_reply_id
                        .clone()
                        .ok_or("this notification takes no reply")?;
                    daemon.send(DaemonCommand::ReplyNotification(
                        device_id, reply_id, argument,
                    ))
                }
                "NotificationDismiss" => daemon.send(DaemonCommand::DismissNotification(
                    device_id,
                    notification_id,
                )),
                _ => return Err("not an action this notification offers".to_string()),
            }
            Ok(())
        }
        "OpenTransfer" => {
            let id = params.get::<(String,)>().unwrap_or_default().0;
            // Only a file on the recent list, never an arbitrary path.
            let file = crate::device_links::transfer_log::recent()
                .into_iter()
                .take(RECENT_TRANSFERS)
                .find(|file| file.path.to_string_lossy() == id)
                .ok_or("not a recent transfer")?;
            let uri = gio::File::for_path(&file.path).uri();
            gio::AppInfo::launch_default_for_uri(&uri, gio::AppLaunchContext::NONE)
                .map_err(|error| error.to_string())
        }
        _ => Err("unknown method".to_string()),
    }
}

fn perform(app: &DesklinkApplication, device: Option<&ShellDevice>, action: &str) {
    let id = device.map(|device| device.id.clone());
    let daemon = app.daemon_handle();
    match (action, id, daemon) {
        ("ring", Some(id), Some(daemon)) => daemon.send(DaemonCommand::SendFindPhone(id)),
        ("lock", Some(id), Some(daemon)) => daemon.send(DaemonCommand::SendLockRequest(id, true)),
        ("media-play-pause" | "media-next", Some(id), Some(daemon)) => {
            let player = daemon
                .devices()
                .into_iter()
                .find(|known| known.id == id)
                .and_then(|known| known.media_status)
                .map(|media| media.player);
            if let Some(player) = player {
                let verb = if action == "media-next" {
                    "Next"
                } else {
                    "PlayPause"
                };
                daemon.send(DaemonCommand::SendMprisAction(id, player, verb.into()));
            }
        }
        ("send-clipboard", Some(id), Some(daemon)) => daemon.send(DaemonCommand::SendClipboard(id)),
        (
            "call-answer" | "call-decline" | "call-silence" | "call-end" | "call-mute"
            | "call-unmute",
            Some(id),
            Some(daemon),
        ) => daemon.send(DaemonCommand::Telephony(
            id,
            action.trim_start_matches("call-").to_string(),
            None,
        )),
        ("try-again", Some(id), Some(daemon)) => daemon.send(DaemonCommand::Reconnect(id)),
        ("reconnect", _, Some(daemon)) => {
            daemon.send(DaemonCommand::NetworkChanged);
            daemon.send(DaemonCommand::Discover);
        }
        ("end-sessions", id, Some(daemon)) => daemon.send(DaemonCommand::EndPhoneSessions(id)),
        ("lock-hand-off", _, Some(daemon)) => daemon.send(DaemonCommand::LockAndHandOff),
        ("keep-awake-on" | "keep-awake-off", _, Some(daemon)) => {
            daemon.send(DaemonCommand::SetStayAwake(action == "keep-awake-on"))
        }
        ("clipboard-on" | "clipboard-off", _, Some(daemon)) => {
            daemon.send(DaemonCommand::SetClipboardSync(action == "clipboard-on"))
        }
        ("unlock-via-phone", Some(id), _) => {
            // Reads the lock state (loginctl): off the main loop. Every
            // check is made again there and when the phone answers.
            std::thread::spawn(move || {
                crate::device_links::desk_features::offer_unlock(&id, "button");
            });
        }
        ("new-terminal", _, _) => {
            if let Err(error) = crate::device_links::pty::desk::new_desk_session() {
                eprintln!("[Shell] Could not start a terminal: {error}");
            }
        }
        ("send-files", Some(id), Some(_)) => {
            let name = device
                .map(|device| device.name.as_str())
                .unwrap_or_default();
            pick_and_send_files(app, &id, name);
        }
        ("open-bucket", Some(id), _) => {
            app.activate_action("open-bucket", Some(&id.to_variant()));
        }
        ("new-note", _, _) => {
            let window = app.main_window();
            window.present();
            window.new_note();
        }
        ("pair-new", _, _) => {
            if let Some(window) = main_window(app) {
                let _ = window.activate_action("win.pair-new", None);
            }
        }
        // "open"
        _ => {
            let window = main_window(app);
            if let (Some(window), Some(device)) =
                (window.and_downcast::<crate::DesklinkWindow>(), device)
            {
                window.show_device(&device.id);
            }
        }
    }
}

/// Asks for files in DeskLink's own dialog and sends them to `device_id`.
pub(crate) fn pick_and_send_files(app: &DesklinkApplication, device_id: &str, name: &str) {
    let Some(daemon) = app.daemon_handle() else {
        return;
    };
    let window = main_window(app);
    let id = device_id.to_string();
    gtk::FileDialog::builder()
        .title(format!("Send Files to {name}"))
        .modal(true)
        .build()
        .open_multiple(window.as_ref(), gio::Cancellable::NONE, move |files| {
            let Ok(files) = files else { return };
            for file in files.iter::<gio::File>().flatten() {
                if let Some(path) = file.path() {
                    daemon.send(DaemonCommand::SendFile(id.clone(), path));
                }
            }
        });
}

/// Presents the main window (creating it if needed) and returns it.
fn main_window(app: &DesklinkApplication) -> Option<gtk::Window> {
    app.activate();
    app.active_window()
}

const EXTENSION_UUID: &str = "desklink@derx06.github.io";
const SHELL_EXTENSIONS: &str = "org.gnome.Shell.Extensions";
const SHELL_EXTENSIONS_PATH: &str = "/org/gnome/Shell/Extensions";
/// `ExtensionState.ACTIVE` (called `ENABLED` before GNOME 46).
const EXTENSION_ACTIVE: f64 = 1.0;

pub(crate) fn is_gnome(current_desktop: &str) -> bool {
    current_desktop
        .split(':')
        .any(|desktop| desktop.eq_ignore_ascii_case("GNOME"))
}

fn call_extensions(
    connection: &gio::DBusConnection,
    method: &str,
    uuid: &str,
    done: impl FnOnce(Result<Variant, glib::Error>) + 'static,
) {
    connection.call(
        Some(SHELL_EXTENSIONS),
        SHELL_EXTENSIONS_PATH,
        SHELL_EXTENSIONS,
        method,
        Some(&(uuid,).to_variant()),
        None,
        // The Shell's D-Bus-activated extensions service, meant for apps.
        gio::DBusCallFlags::NONE,
        // Installing waits for the user to answer GNOME's own dialog.
        i32::MAX,
        gio::Cancellable::NONE,
        done,
    );
}

/// The `a{sv}` of `GetExtensionInfo`, empty when not installed.
fn extension_info(reply: &Variant) -> glib::VariantDict {
    glib::VariantDict::new(
        reply
            .try_child_value(0)
            .filter(|v| v.type_() == glib::VariantTy::VARDICT)
            .as_ref(),
    )
}

fn is_active(info: &glib::VariantDict) -> bool {
    info.lookup::<f64>("state").ok().flatten() == Some(EXTENSION_ACTIVE)
}

/// The GNOME extensions that host StatusNotifier tray icons.
const APPINDICATOR_UUIDS: [&str; 2] = [
    "appindicatorsupport@rgcjonas.gmail.com",
    "ubuntu-appindicators@ubuntu.com",
];

thread_local! {
    static EXTENSION_IS_ACTIVE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// GNOME Shell has loaded ours (it may still be off): `GetExtensionInfo`
    /// is not empty. A copy written this session is not loaded until the
    /// next login on Wayland.
    static EXTENSION_KNOWN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// An installed AppIndicator extension, and whether it is on.
    static APPINDICATOR: RefCell<Option<(String, bool)>> = const { RefCell::new(None) };
}

/// The top bar on this desktop, for Settings.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub enum ExtensionStatus {
    Active,
    /// Written for this user; GNOME loads it at the next login.
    NeedsLogin,
    Off,
}

pub fn extension_status() -> ExtensionStatus {
    let installed = || user_extension_dir().join("metadata.json").is_file();
    if extension_active() {
        ExtensionStatus::Active
    } else if !EXTENSION_KNOWN.with(|known| known.get()) && installed() {
        ExtensionStatus::NeedsLogin
    } else {
        ExtensionStatus::Off
    }
}

/// An installed AppIndicator extension that is off (its uuid): turning it on
/// shows DeskLink's tray icon at once, no login needed.
pub fn appindicator_off() -> Option<String> {
    APPINDICATOR.with(|found| {
        found
            .borrow()
            .as_ref()
            .filter(|(_, on)| !on)
            .map(|(uuid, _)| uuid.clone())
    })
}

fn user_extension_dir() -> std::path::PathBuf {
    glib::user_data_dir()
        .join("gnome-shell")
        .join("extensions")
        .join(EXTENSION_UUID)
}

/// Turns on an installed extension by `uuid`, user extensions first (the
/// user's own click only).
pub fn enable_extension(uuid: &str, done: impl FnOnce(Result<(), String>) + 'static) {
    let settings = gio::Settings::new("org.gnome.shell");
    if settings.boolean("disable-user-extensions") {
        if let Err(error) = settings.set_boolean("disable-user-extensions", false) {
            return done(Err(error.to_string()));
        }
        gio::Settings::sync();
    }
    let Ok(connection) = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE) else {
        return done(Err("No session bus".into()));
    };
    call_extensions(&connection, "EnableExtension", uuid, move |reply| {
        done(match reply {
            Ok(reply)
                if reply.try_child_value(0).and_then(|ok| ok.get::<bool>()) == Some(false) =>
            {
                Err("GNOME did not turn it on".into())
            }
            Ok(_) => Ok(()),
            Err(error) => Err(error.message().to_string()),
        })
    });
}

/// Asks GNOME to log out; GNOME itself asks the user to confirm.
pub fn log_out() {
    let Ok(connection) = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE) else {
        return;
    };
    connection.call(
        Some("org.gnome.SessionManager"),
        "/org/gnome/SessionManager",
        "org.gnome.SessionManager",
        "Logout",
        // 0: the normal logout, with GNOME's confirmation dialog.
        Some(&(0u32,).to_variant()),
        None,
        gio::DBusCallFlags::NONE,
        -1,
        gio::Cancellable::NONE,
        |reply| {
            if let Err(error) = reply {
                eprintln!("[Shell] Could not log out: {error}");
            }
        },
    );
}

/// The top-bar client that said `Hello`: its bus name, what it handles, and
/// the watch that forgets it when it goes.
struct Client {
    capabilities: Vec<String>,
    /// Stops the watch (its id type is not nameable outside `gio`).
    unwatch: Box<dyn FnOnce()>,
}

thread_local! {
    static CLIENT: RefCell<Option<Client>> = const { RefCell::new(None) };
}

/// What a top-bar client may say it handles.
const KNOWN_CAPABILITIES: [&str; 3] = ["notifications", "quick-settings", "calls"];

/// Only what DeskLink knows, at most once each.
fn vetted(capabilities: Vec<String>) -> Vec<String> {
    let mut kept: Vec<String> = Vec::new();
    for capability in capabilities {
        if KNOWN_CAPABILITIES.contains(&capability.as_str()) && !kept.contains(&capability) {
            kept.push(capability);
        }
    }
    kept
}

/// Whether `sender` (a unique bus name) is the owner of `org.gnome.Shell`.
fn is_the_shell(sender: &str, shell_owner: Option<&str>) -> bool {
    !sender.is_empty() && shell_owner == Some(sender)
}

fn hello_is_from_the_shell(connection: &gio::DBusConnection, sender: &str) -> bool {
    let owner = connection
        .call_sync(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "GetNameOwner",
            Some(&("org.gnome.Shell",).to_variant()),
            None,
            gio::DBusCallFlags::NONE,
            500,
            gio::Cancellable::NONE,
        )
        .ok()
        .and_then(|reply| reply.get::<(String,)>())
        .map(|(owner,)| owner);
    is_the_shell(sender, owner.as_deref())
}

/// A top-bar client (the extension, on enable) says what it handles. Kept
/// until its bus name goes away (Shell restart, extension disabled).
fn client_said_hello(connection: &gio::DBusConnection, sender: &str, capabilities: Vec<String>) {
    let watch = gio::bus_watch_name_on_connection(
        connection,
        sender,
        gio::BusNameWatcherFlags::NONE,
        |_, _, _| {},
        |_, _| forget_client(),
    );
    let unwatch: Box<dyn FnOnce()> = Box::new(move || gio::bus_unwatch_name(watch));
    let previous = CLIENT.with(|client| {
        client.replace(Some(Client {
            capabilities,
            unwatch,
        }))
    });
    if let Some(previous) = previous {
        (previous.unwatch)();
    }
}

fn forget_client() {
    if let Some(previous) = CLIENT.with(|client| client.take()) {
        (previous.unwatch)();
    }
}

/// Whether a running top-bar client said it shows `capability` (e.g.
/// `notifications`). An older extension says nothing, so the app keeps
/// showing them itself until the new one is running.
pub fn client_handles(capability: &str) -> bool {
    CLIENT.with(|client| {
        client
            .borrow()
            .as_ref()
            .is_some_and(|client| client.capabilities.iter().any(|known| known == capability))
    })
}

/// Whether DeskLink's top-bar extension is running, so it already shows
/// sessions (kept current from `ExtensionStateChanged`, not polled).
pub fn extension_active() -> bool {
    EXTENSION_IS_ACTIVE.with(|active| active.get())
}

/// The extension, built into the app so any install can turn it on.
const EXTENSION_FILES: [(&str, &str); 6] = [
    (
        "metadata.json",
        include_str!("../gnome-shell-extension/desklink@derx06.github.io/metadata.json"),
    ),
    (
        "extension.js",
        include_str!("../gnome-shell-extension/desklink@derx06.github.io/extension.js"),
    ),
    (
        "notifications.js",
        include_str!("../gnome-shell-extension/desklink@derx06.github.io/notifications.js"),
    ),
    (
        "stylesheet.css",
        include_str!("../gnome-shell-extension/desklink@derx06.github.io/stylesheet.css"),
    ),
    (
        "menu.js",
        include_str!("../gnome-shell-extension/desklink@derx06.github.io/menu.js"),
    ),
    (
        "desklink-symbolic.svg",
        include_str!("../gnome-shell-extension/desklink@derx06.github.io/desklink-symbolic.svg"),
    ),
];

/// Whether this desktop can show the top-bar menu at all.
pub fn extension_possible() -> bool {
    // A Flatpak cannot install into the host's shell: no offer, no failure.
    !crate::device_links::pty::host::in_flatpak()
        && is_gnome(&std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default())
        && gio::SettingsSchemaSource::default()
            .and_then(|source| source.lookup("org.gnome.shell", true))
            .is_some()
}

/// Beside the installed extension: the digest of what DeskLink wrote, so a
/// copy the user changed is told apart from an older one of ours.
const INSTALL_STAMP: &str = ".desklink-installed";

/// SHA-256 over each extension file's name and contents (missing: empty).
fn files_digest<'a>(files: impl Iterator<Item = (&'a str, Option<&'a str>)>) -> String {
    let mut hasher = openssl::sha::Sha256::new();
    for (name, content) in files {
        hasher.update(name.as_bytes());
        hasher.update(&[0]);
        hasher.update(content.unwrap_or_default().as_bytes());
        hasher.update(&[0]);
    }
    hasher
        .finish()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Every extension DeskLink has shipped, by [`files_digest`] over the
/// `EXTENSION_FILES` names (a file a build lacked counts as empty), from the
/// git history of `gnome-shell-extension/` with
/// `~/.cache/desklink-work/ext-digests.py`: a copy installed by hand, with no
/// stamp, is still ours and safe to update.
const KNOWN_BUILDS: [&str; 13] = [
    // The current build (w7c: no images decoded in the shell, follow-up
    // messages announced, the reply dialog destroyed, accent fallback).
    "73de6db35cf56eba8b413c3cca78d2a7389efdb1f3653f5178a02b67d8864607",
    // W4: the Files row, Settings in its comments.
    "6a907b946c22e452a0853b683a98bef425482d0fe38181d4dce255e4cb7fc8da",
    // The Bucket's sync progress row.
    "01fee36c261108ac481f15db17de0b7c860e79ba8e77d8a932540e07952a7f8a",
    // 35a2ff3, F4: glide in the live row.
    "1ce14fd319e5ac1654c16dfa8a79910c95da9bdcca33f880f5f3f62654de440d",
    // F6: Snippets and New Note in the menu.
    "d15f25a0eaca4d3f37d6423d5eefafeadacd0dc3fb8e6c07389ce7861d701f50",
    // Top-bar redesign: menu.js, the glyph icon.
    "299c2cbb85734c4379c74cf5afd4e6d3a73f3d9ab9fb4cc1738de0b165297da5",
    // be1b760, 2026-10-21.
    "fe350711d70ada59bc0428004409555265854e5c91d5cda444439823ac7707b2",
    // 9784a7d, 2026-10-21.
    "14c40f57d58fe269429b3969aa555fbfc85b0afe9787fbfb30feb621a4d1f0cb",
    // f798285, 2026-10-21.
    "befac3aedecccb11f26d2a19b4eeb9f6b59638b6d09387ad3a4a01cbbb548d78",
    // 1b78f08, 2026-10-21.
    "09c6a14190617bb0c7c32ef7ef5f048d1a4e1abaed8b2b8146f658648d48d6cf",
    // ba03b84, 2026-10-21.
    "9e3afd7a15663156d6377aed4acfbc9ac5f9ec9af11aee725831d8e380d5c2f4",
    // 635fc5d, 2026-10-20.
    "8598af7cdd26d69961816c7d215146566a4a975e83d3d6fbbd5e0de414225402",
    // 1f8c0a4, 2026-10-20.
    "85e45fc9fdd19dc3b711ca70df8ae7d1600bf2e252eccfd0e9a2550f99cc42af",
];

/// Whether an installed copy (by digest) is DeskLink's own and may be
/// replaced: the stamp we wrote says so, or it is a build we shipped.
fn ours(digest: &str, stamped: bool, known: &[&str]) -> bool {
    stamped || known.contains(&digest)
}

/// Writes `path` whole or not at all.
fn write_atomically(path: &std::path::Path, content: &str) -> Result<(), String> {
    let temporary = path.with_extension("desklink-tmp");
    std::fs::write(&temporary, content).map_err(|error| error.to_string())?;
    std::fs::rename(&temporary, path).map_err(|error| error.to_string())
}

/// Writes the extension into `dir` (`~/.local/share/gnome-shell/extensions/<uuid>`)
/// when missing or an older copy of ours. Whether anything was written. A
/// copy someone changed is left alone, with an error saying so.
fn install_extension_files(dir: &std::path::Path) -> Result<bool, String> {
    let present: Vec<(&str, Option<String>)> = EXTENSION_FILES
        .iter()
        .map(|(name, _)| (*name, std::fs::read_to_string(dir.join(name)).ok()))
        .collect();
    let current = files_digest(present.iter().map(|(name, have)| (*name, have.as_deref())));
    let embedded = files_digest(
        EXTENSION_FILES
            .iter()
            .map(|(name, content)| (*name, Some(*content))),
    );
    let stamp = std::fs::read_to_string(dir.join(INSTALL_STAMP)).ok();
    let stamped = stamp.as_deref().map(str::trim) == Some(current.as_str());
    if current == embedded {
        // Ours already (perhaps copied by hand): stamped, so a later build
        // knows it may update it.
        if !stamped {
            write_atomically(&dir.join(INSTALL_STAMP), &embedded)?;
        }
        return Ok(false);
    }
    if present.iter().any(|(_, have)| have.is_some()) && !ours(&current, stamped, &KNOWN_BUILDS) {
        return Err(format!(
            "A changed copy of the DeskLink extension is in {}; it was left as it is",
            dir.display()
        ));
    }
    std::fs::create_dir_all(dir).map_err(|error| error.to_string())?;
    for (name, content) in EXTENSION_FILES {
        write_atomically(&dir.join(name), content)?;
    }
    write_atomically(&dir.join(INSTALL_STAMP), &embedded)?;
    Ok(true)
}

/// `enabled-extensions` with ours added, once.
fn with_extension(enabled: Vec<String>) -> Vec<String> {
    let mut enabled = enabled;
    if !enabled.iter().any(|uuid| uuid == EXTENSION_UUID) {
        enabled.push(EXTENSION_UUID.to_string());
    }
    enabled
}

/// "Turn on top-bar menu", only ever from the user's click: installs the
/// built-in extension for this user (when missing or outdated), turns user
/// extensions on and enables it. `Ok(true)` when the Shell must be restarted
/// (on Wayland: log out and back in) before it shows; activation is seen by
/// [`track_extension`].
pub fn turn_on_extension() -> Result<bool, String> {
    if !extension_possible() {
        return Err("The top-bar menu needs GNOME".into());
    }
    let written = install_extension_files(&user_extension_dir())?;
    let settings = gio::Settings::new("org.gnome.shell");
    settings
        .set_boolean("disable-user-extensions", false)
        .map_err(|error| error.to_string())?;
    let strings = |key: &str| -> Vec<String> {
        settings
            .strv(key)
            .iter()
            .map(|uuid| uuid.to_string())
            .collect()
    };
    settings
        .set_strv(
            "enabled-extensions",
            with_extension(strings("enabled-extensions")),
        )
        .map_err(|error| error.to_string())?;
    let disabled: Vec<String> = strings("disabled-extensions")
        .into_iter()
        .filter(|uuid| uuid != EXTENSION_UUID)
        .collect();
    settings
        .set_strv("disabled-extensions", disabled)
        .map_err(|error| error.to_string())?;
    gio::Settings::sync();
    Ok(written && !extension_active())
}

fn track_extension(connection: &gio::DBusConnection) {
    if !is_gnome(&std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default()) {
        return;
    }
    let set = |active: bool| {
        EXTENSION_IS_ACTIVE.with(|cell| cell.set(active));
        EXTENSION_KNOWN.with(|cell| cell.set(true));
    };
    // Broadcast by the Shell (and the extensions service); any sender.
    connection.signal_subscribe(
        None,
        Some(SHELL_EXTENSIONS),
        Some("ExtensionStateChanged"),
        None,
        Some(EXTENSION_UUID),
        gio::DBusSignalFlags::NONE,
        move |_, _, _, _, _, params| {
            crate::guard("top-bar signal", (), || {
                set(is_active(&glib::VariantDict::new(
                    params
                        .try_child_value(1)
                        .filter(|v| v.type_() == glib::VariantTy::VARDICT)
                        .as_ref(),
                )))
            })
        },
    );
    call_extensions(
        connection,
        "GetExtensionInfo",
        EXTENSION_UUID,
        move |reply| {
            if let Ok(reply) = reply {
                let info = extension_info(&reply);
                EXTENSION_IS_ACTIVE.with(|cell| cell.set(is_active(&info)));
                EXTENSION_KNOWN.with(|cell| cell.set(info.lookup_value("uuid", None).is_some()));
            }
        },
    );
    // The tray hosts: which one is installed, and whether it is on.
    for uuid in APPINDICATOR_UUIDS {
        let note = move |info: &glib::VariantDict| {
            if info.lookup_value("uuid", None).is_some() {
                APPINDICATOR.with(|found| {
                    found.replace(Some((uuid.to_string(), is_active(info))));
                });
            }
        };
        connection.signal_subscribe(
            None,
            Some(SHELL_EXTENSIONS),
            Some("ExtensionStateChanged"),
            None,
            Some(uuid),
            gio::DBusSignalFlags::NONE,
            move |_, _, _, _, _, params| {
                crate::guard("top-bar signal", (), || {
                    note(&glib::VariantDict::new(
                        params
                            .try_child_value(1)
                            .filter(|v| v.type_() == glib::VariantTy::VARDICT)
                            .as_ref(),
                    ))
                })
            },
        );
        call_extensions(connection, "GetExtensionInfo", uuid, move |reply| {
            if let Ok(reply) = reply {
                note(&extension_info(&reply));
            }
        });
    }
}

/// Shown after turning the menu on, when GNOME has to load it first.
pub const RELOGIN_NOTE: &str =
    "GNOME shows a newly added top-bar menu after you log out and back in (on X11, restarting GNOME Shell is enough).";

/// On GNOME with no tray host and our extension not running, offers it in
/// one notification ("Show" / "Not now") a little after start, until the user
/// says "Not now".
fn offer_extension(app: &DesklinkApplication) {
    app.add_action_entries([
        // The notification's "Turn On": the user's click.
        gio::ActionEntry::builder("shell-extension-show")
            .activate(|app: &DesklinkApplication, _, _| {
                let notification = match turn_on_extension() {
                    Ok(false) => return,
                    Ok(true) => {
                        let notification = gio::Notification::new("Top-bar menu turned on");
                        notification.set_body(Some(RELOGIN_NOTE));
                        notification.add_button("Log Out Now", "app.log-out");
                        notification
                    }
                    Err(error) => {
                        eprintln!("[Shell] Could not turn on the top-bar menu: {error}");
                        let notification =
                            gio::Notification::new("Could not turn on the top-bar menu");
                        notification.set_body(Some(&error));
                        notification
                    }
                };
                app.send_notification(Some("shell-extension"), &notification);
            })
            .build(),
        // "Turn On" for an installed AppIndicator extension: the tray icon
        // shows at once.
        gio::ActionEntry::builder("tray-host-on")
            .parameter_type(Some(glib::VariantTy::STRING))
            .activate(|app: &DesklinkApplication, _, uuid| {
                let Some(uuid) = uuid.and_then(|uuid| uuid.get::<String>()) else {
                    return;
                };
                if !APPINDICATOR_UUIDS.contains(&uuid.as_str()) {
                    return;
                }
                let app = app.clone();
                enable_extension(&uuid, move |result| {
                    app.withdraw_notification("shell-extension");
                    if let Err(error) = result {
                        let notification =
                            gio::Notification::new("Could not show DeskLink in the top bar");
                        notification.set_body(Some(&error));
                        app.send_notification(Some("shell-extension"), &notification);
                    }
                });
            })
            .build(),
        gio::ActionEntry::builder("log-out")
            .activate(|_: &DesklinkApplication, _, _| log_out())
            .build(),
        gio::ActionEntry::builder("shell-extension-dismiss")
            .activate(|app: &DesklinkApplication, _, _| {
                if let Some(daemon) = app.daemon_handle() {
                    daemon.send(DaemonCommand::SetShellExtensionPromptDismissed(true));
                }
            })
            .build(),
    ]);

    // An installed copy of ours from an older DeskLink: brought up to date
    // (it was turned on before, so this is no new choice), and the user told
    // when it shows. Once registration has finished: during `dbus_register`
    // the application is not registered yet, so `is_remote` and
    // `send_notification` only assert.
    let weak = app.downgrade();
    glib::idle_add_local_once(move || {
        let Some(app) = weak.upgrade() else { return };
        if app.is_remote()
            || !is_gnome(&std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default())
            || !user_extension_dir().join("metadata.json").is_file()
        {
            return;
        }
        match install_extension_files(&user_extension_dir()) {
            Ok(true) => {
                let notification = gio::Notification::new("DeskLink's top-bar menu was updated");
                notification.set_body(Some(RELOGIN_NOTE));
                notification.add_button("Log Out Now", "app.log-out");
                app.send_notification(Some("shell-extension"), &notification);
            }
            Ok(false) => {}
            Err(error) => eprintln!("[Shell] {error}"),
        }
    });

    let weak = app.downgrade();
    // Late enough for the tray to have found any host.
    glib::timeout_add_seconds_local_once(10, move || {
        let Some(app) = weak.upgrade() else { return };
        let wanted = !app.is_remote()
            && !crate::device_links::pty::host::in_flatpak()
            && is_gnome(&std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default())
            && !crate::device_links::config::app_settings().shell_extension_prompt_dismissed
            && !app.tray_hosted();
        let Some(connection) = app.dbus_connection().filter(|_| wanted) else {
            return;
        };
        // An AppIndicator extension that is only off: the one-click fix
        // (not needed while our own extension shows DeskLink).
        if let Some(uuid) = appindicator_off().filter(|_| !extension_active()) {
            let notification = gio::Notification::new("Show DeskLink in the top bar");
            notification.set_body(Some(
                "Turn on the AppIndicator extension that is already installed to see DeskLink's icon in the top bar.",
            ));
            notification.add_button_with_target_value(
                "Turn On",
                "app.tray-host-on",
                Some(&uuid.to_variant()),
            );
            notification.add_button("Not now", "app.shell-extension-dismiss");
            app.send_notification(Some("shell-extension"), &notification);
            return;
        }
        // Fails when the Shell's extension service is absent: no offer then.
        call_extensions(
            &connection,
            "GetExtensionInfo",
            EXTENSION_UUID,
            move |reply| {
                let Ok(reply) = reply else { return };
                if is_active(&extension_info(&reply)) {
                    return;
                }
                let notification = gio::Notification::new("Show DeskLink in the top bar");
                notification.set_body(Some(
                    "See your phone's battery and connection, and act on it, from the top bar.",
                ));
                notification.add_button("Turn On", "app.shell-extension-show");
                notification.add_button("Not now", "app.shell-extension-dismiss");
                app.send_notification(Some("shell-extension"), &notification);
            },
        );
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device_links::device::BatteryStatus;
    use crate::device_links::device_info::DeviceInfo;

    /// Unlock with the phone on for `phone1` only.
    fn on() -> AppSettings {
        let mut settings = AppSettings::default();
        settings.unlock_devices.insert("phone1".into());
        settings
    }

    fn phone() -> DeviceView {
        let info = DeviceInfo {
            id: "phone1".into(),
            name: "Pixel".into(),
            device_type: "phone".into(),
            protocol_version: 9,
            incoming_capabilities: Vec::new(),
            outgoing_capabilities: Vec::new(),
            instance_id: None,
        };
        DeviceView::from_info(&info, "10.0.0.2".into(), true)
    }

    #[test]
    fn a_bucket_sync_is_published_with_its_line_detail_and_fraction() {
        let _shared = crate::bucket_progress::test_lock();
        use crate::device_links::webrtc::transfer_board::TransferRow;
        let mut device = phone();
        device.id = "shell-bucket".into();
        assert_eq!(shell_device(&device, &AppSettings::default()).bucket, None);
        crate::bucket_progress::sample_for_test(vec![TransferRow {
            id: "shell-bucket-1".into(),
            device_id: "shell-bucket".into(),
            name: "film.mkv".into(),
            incoming: true,
            done: 300_000_000,
            total: 1_200_000_000,
            preparing: false,
            paused: false,
        }]);
        let mapped = shell_device(&device, &AppSettings::default());
        let bucket = mapped.bucket.clone().expect("published while syncing");
        let expected = format!(
            "Syncing 1 file · {} of {}",
            crate::bucket_progress::size_words(300_000_000),
            crate::bucket_progress::size_words(1_200_000_000)
        );
        assert_eq!(bucket.line, expected);
        assert_eq!(bucket.detail, "Waiting…", "no speed after one sample");
        assert_eq!(bucket.permille, 250);
        let list = devices_variant(&[mapped]);
        let dict = glib::VariantDict::new(Some(&list.child_value(0)));
        let entry = glib::VariantDict::new(dict.lookup_value("bucket", None).as_ref());
        assert_eq!(
            entry.lookup::<String>("line").unwrap().as_deref(),
            Some(expected.as_str())
        );
        assert_eq!(entry.lookup::<f64>("fraction").unwrap(), Some(0.25));
        crate::bucket_progress::sample_for_test(Vec::new());
        assert_eq!(shell_device(&device, &AppSettings::default()).bucket, None);
    }

    #[test]
    fn a_device_maps_to_the_documented_dict() {
        let mut device = phone();
        device.status = DeviceStatus::Paired;
        device.feature_transport_ready = true;
        device.connection_path = Some("lan".into());
        device.connection_nearby = true;
        device.desktop_session = DesktopSession::Controlling;
        device.terminal_sessions_attached = 2;
        device.battery_status = Some(BatteryStatus {
            current_charge: 81,
            is_charging: true,
            battery_quantity: None,
            threshold_event: None,
            charge_time_remaining: None,
        });
        let mapped = shell_device(&device, &AppSettings::default());
        assert!(mapped.paired && mapped.reachable && mapped.ready && mapped.charging);
        assert_eq!(
            (mapped.battery, mapped.screen, mapped.terminals),
            (81, "control", 2)
        );
        assert_eq!(mapped.path, "lan");

        let list = devices_variant(&[mapped]);
        assert_eq!(list.type_().as_str(), "aa{sv}");
        let dict = glib::VariantDict::new(Some(&list.child_value(0)));
        assert_eq!(
            dict.lookup::<String>("id").unwrap().as_deref(),
            Some("phone1")
        );
        assert_eq!(
            dict.lookup::<String>("type").unwrap().as_deref(),
            Some("phone")
        );
        assert_eq!(dict.lookup::<i32>("battery").unwrap(), Some(81));
        assert_eq!(dict.lookup::<u32>("terminals").unwrap(), Some(2));
        assert_eq!(
            dict.lookup::<Vec<String>>("actions").unwrap().unwrap(),
            [
                "open",
                "ring",
                "lock",
                "send-files",
                "send-clipboard",
                "new-terminal",
                "end-sessions"
            ]
        );

        let idle = shell_device(&phone(), &AppSettings::default());
        assert!(!idle.reachable && !idle.ready);
        assert_eq!(
            (idle.battery, idle.screen, idle.path.as_str()),
            (-1, "", "")
        );
    }

    #[test]
    fn a_ringing_call_is_published_with_answer_and_decline() {
        use crate::device_links::device::{CallState, CallStatus, PhoneCall};
        let mut device = phone();
        device.status = DeviceStatus::Paired;
        device.feature_transport_ready = true;
        device.incoming_capabilities =
            vec![crate::device_links::packet::PACKET_TYPE_TELEPHONY_REQUEST.into()];
        device.call_status = Some(CallStatus {
            calls: vec![PhoneCall {
                call_id: "c1".into(),
                state: CallState::Ringing,
                contact_name: None,
                number: None,
                started_at: None,
                incoming: true,
            }],
            muted: false,
            silenced: false,
        });
        let mapped = shell_device(&device, &AppSettings::default());
        for action in ["call-answer", "call-decline", "call-silence"] {
            assert!(mapped.actions.contains(&action), "{action}");
        }
        let devices = [mapped.clone()];
        assert!(check(&devices, "phone1", "call-answer").is_ok());
        assert!(
            check(&devices, "phone1", "call-end").is_err(),
            "nothing to end"
        );
        let dict = glib::VariantDict::new(Some(&devices_variant(&devices).child_value(0)));
        let call = glib::VariantDict::new(Some(&dict.lookup_value("call", None).unwrap()));
        assert_eq!(
            call.lookup::<String>("state").unwrap().as_deref(),
            Some("ringing")
        );
        assert_eq!(
            call.lookup::<String>("caller").unwrap().as_deref(),
            Some("Incoming call"),
            "a private caller"
        );
        assert_eq!(call.lookup::<bool>("incoming").unwrap(), Some(true));

        device.feature_transport_ready = false;
        let stale = shell_device(&device, &AppSettings::default());
        assert!(stale.call.is_none() && !stale.actions.iter().any(|a| a.starts_with("call-")));
    }

    #[test]
    fn actions_follow_readiness_and_sessions_and_invoke_checks_them() {
        let mut device = phone();
        assert_eq!(actions(&device, &on()), ["open"]);
        device.terminal_sessions_attached = 1;
        assert_eq!(actions(&device, &on()), ["open", "end-sessions"]);

        let devices = [shell_device(&device, &on())];
        assert!(check(&devices, "phone1", "open").unwrap().is_some());
        assert!(check(&devices, "phone1", "end-sessions").is_ok());
        assert!(check(&devices, "phone1", "ring").is_err(), "not ready");
        assert!(check(&devices, "phone1", "explode").is_err());
        assert!(check(&devices, "nobody", "open").is_err());
        assert!(check(&devices, "", "pair-new").unwrap().is_none());
        assert!(check(&devices, "", "end-sessions").is_ok());
        assert!(check(&devices, "", "ring").is_err());
        assert!(check(&devices, "", "lock-hand-off").is_ok());
        assert!(
            check(&devices, "phone1", "unlock-via-phone").is_err(),
            "not connected"
        );
    }

    #[test]
    fn the_unlock_button_needs_the_setting_and_a_phone_on_this_network() {
        let mut device = phone();
        device.status = DeviceStatus::Paired;
        device.feature_transport_ready = true;
        device.connection_path = Some("lan".into());
        device.connection_nearby = true;
        assert!(actions(&device, &on()).contains(&"unlock-via-phone"));
        assert!(
            !actions(&device, &AppSettings::default()).contains(&"unlock-via-phone"),
            "setting off"
        );
        let mut other = AppSettings::default();
        other.unlock_devices.insert("another-phone".into());
        assert!(
            !actions(&device, &other).contains(&"unlock-via-phone"),
            "on for another phone only"
        );
        for path in ["direct", "turn", "relay"] {
            device.connection_path = Some(path.into());
            assert!(
                !actions(&device, &on()).contains(&"unlock-via-phone"),
                "{path}"
            );
        }
    }

    #[test]
    fn only_state_1_counts_as_an_active_extension() {
        let info = |state: f64| {
            let dict = glib::VariantDict::new(None);
            dict.insert("state", state);
            dict
        };
        assert!(is_active(&info(1.0)));
        assert!(!is_active(&info(2.0)));
        assert!(!is_active(&info(6.0)));
        assert!(!is_active(&glib::VariantDict::new(None)));
    }

    #[test]
    fn a_copy_is_ours_when_stamped_or_a_shipped_build() {
        let known = ["aaa", "bbb"];
        assert!(ours("zzz", true, &known), "stamped by us");
        assert!(
            ours("bbb", false, &known),
            "copied by hand, a shipped build"
        );
        assert!(!ours("zzz", false, &known), "edited by someone");
        // Every build is listed, so a hand copy of this one is known later:
        // changing the extension means adding its digest to KNOWN_BUILDS.
        let embedded = files_digest(
            EXTENSION_FILES
                .iter()
                .map(|(name, content)| (*name, Some(*content))),
        );
        assert!(
            KNOWN_BUILDS.contains(&embedded.as_str()),
            "add {embedded} to KNOWN_BUILDS"
        );
        assert!(
            KNOWN_BUILDS
                .contains(&"8598af7cdd26d69961816c7d215146566a4a975e83d3d6fbbd5e0de414225402"),
            "the 2026-10-20 build (extension.js 6911 bytes)"
        );
    }

    #[test]
    fn the_extension_is_written_once_and_enabled_once() {
        let dir = std::env::temp_dir().join(format!("desklink-ext-{}", uuid::Uuid::new_v4()));
        assert!(install_extension_files(&dir).unwrap(), "missing: written");
        assert!(!install_extension_files(&dir).unwrap(), "same: left alone");
        // An older copy of ours (its stamp matches what is there): replaced.
        std::fs::write(dir.join("extension.js"), "old").unwrap();
        let older = files_digest(EXTENSION_FILES.iter().map(|(name, content)| {
            (
                *name,
                Some(if *name == "extension.js" {
                    "old"
                } else {
                    *content
                }),
            )
        }));
        std::fs::write(dir.join(INSTALL_STAMP), older).unwrap();
        assert!(
            install_extension_files(&dir).unwrap(),
            "outdated: rewritten"
        );
        // A copy someone edited: kept, and the caller is told.
        std::fs::write(dir.join("extension.js"), "// my tweaks").unwrap();
        assert!(install_extension_files(&dir).is_err());
        assert_eq!(
            std::fs::read_to_string(dir.join("extension.js")).unwrap(),
            "// my tweaks"
        );
        let metadata = std::fs::read_to_string(dir.join("metadata.json")).unwrap();
        assert!(metadata.contains(EXTENSION_UUID) && metadata.contains("unlock-dialog"));
        let _ = std::fs::remove_dir_all(&dir);

        // Copied by hand (no stamp) and identical: stamped, nothing written.
        let dir = std::env::temp_dir().join(format!("desklink-ext-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        for (name, content) in EXTENSION_FILES {
            std::fs::write(dir.join(name), content).unwrap();
        }
        assert!(!install_extension_files(&dir).unwrap());
        assert!(dir.join(INSTALL_STAMP).is_file(), "stamped");
        let _ = std::fs::remove_dir_all(&dir);

        let enabled = with_extension(vec!["other@x".into()]);
        assert_eq!(enabled, ["other@x", EXTENSION_UUID]);
        assert_eq!(with_extension(enabled.clone()), enabled);
    }

    #[test]
    fn gnome_is_found_in_a_desktop_list() {
        assert!(is_gnome("GNOME"));
        assert!(is_gnome("ubuntu:GNOME"));
        assert!(!is_gnome("KDE"));
        assert!(!is_gnome(""));
    }

    /// Only the Shell may say it shows DeskLink's alerts (so nobody else on the
    /// bus can mute them), and only for capabilities DeskLink knows.
    #[test]
    fn only_the_shell_may_say_hello_and_only_for_known_capabilities() {
        assert!(is_the_shell(":1.42", Some(":1.42")));
        assert!(!is_the_shell(":1.99", Some(":1.42")), "another client");
        assert!(!is_the_shell(":1.42", None), "no shell on the bus");
        assert!(!is_the_shell("", Some("")));
        let said = vec![
            "notifications".to_string(),
            "calls".to_string(),
            "calls".to_string(),
            "mute-everything".to_string(),
        ];
        assert_eq!(vetted(said), ["notifications", "calls"]);
    }
}
