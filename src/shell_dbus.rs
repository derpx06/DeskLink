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
/// 8: the `insert-photo` action (a photo from the phone onto the clipboard).
/// 9: the complete remote. `Devices[].settings` (per-phone switches),
/// `.sound`, `.rttMs`, `.unread`, `.textCapture`, `.transfers`; more `Desk`
/// switches; the `SetSwitch` and `SendLink` methods; the `ping`, `send-link`,
/// `show-screen`, `media-prev`, `insert-*`, `open-*` and `unpair` actions.
/// The security-sensitive switches can only be turned OFF here
/// ([`SWITCHES`]); turning them on is the application's own page.
const VERSION: u32 = 9;
const INVALID_ARGS: &str = "org.freedesktop.DBus.Error.InvalidArgs";

const XML: &str = r#"<node>
  <interface name="derx06.desklink.com.Shell1">
    <property name="Version" type="u" access="read"/>
    <property name="Devices" type="aa{sv}" access="read"/>
    <property name="Desk" type="a{sv}" access="read"/>
    <property name="Notifications" type="aa{sv}" access="read"/>
    <property name="Transfers" type="aa{sv}" access="read"/>
    <property name="Snippets" type="aa{sv}" access="read"/>
    <property name="Activities" type="aa{sv}" access="read"/>
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
    <method name="SetSwitch">
      <arg name="device_id" type="s" direction="in"/>
      <arg name="key" type="s" direction="in"/>
      <arg name="on" type="b" direction="in"/>
    </method>
    <method name="SendLink">
      <arg name="device_id" type="s" direction="in"/>
      <arg name="link" type="s" direction="in"/>
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
    /// This computer's notifications go to the phones.
    forward_notifications: bool,
    /// This computer's players pause during a call.
    pause_media_on_call: bool,
    /// Phones may hear this computer's microphone.
    desk_mic: bool,
    /// Sound can work here (PipeWire and its GStreamer plugin).
    sound_available: bool,
    /// Do Not Disturb can be shared (GNOME's notification settings).
    dnd_supported: bool,
    /// A session password is set.
    has_password: bool,
}

impl Desk {
    fn to_variant(&self) -> Variant {
        let dict = glib::VariantDict::new(None);
        dict.insert("keepAwake", self.keep_awake);
        dict.insert("clipboard", self.clipboard);
        dict.insert("handOff", self.hand_off);
        dict.insert("pill", self.pill);
        dict.insert("forwardNotifications", self.forward_notifications);
        dict.insert("pauseMediaOnCall", self.pause_media_on_call);
        dict.insert("deskMic", self.desk_mic);
        dict.insert("soundAvailable", self.sound_available);
        dict.insert("dndSupported", self.dnd_supported);
        dict.insert("hasPassword", self.has_password);
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
        forward_notifications: settings.forward_notifications,
        pause_media_on_call: settings.pause_media_on_call,
        desk_mic: settings.desk_mic_to_phone,
        sound_available: crate::platform::reason("sound").is_none(),
        dnd_supported: crate::device_links::dnd::desk_state().is_some(),
        has_password: settings.session_password.is_some(),
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
    /// Its per-phone switches, by [`SWITCHES`] key.
    settings: Vec<(&'static str, bool)>,
    /// Sound with this phone: this computer's to it, its microphone here, its
    /// sound here.
    sound: [bool; 3],
    /// Round trip in milliseconds; -1 unknown.
    rtt_ms: i32,
    /// Its notifications shown here and not yet dismissed.
    unread: u32,
    /// The phone can read text from a picture.
    text_capture: bool,
    /// Files going to or from it now (the Bucket's are in `bucket`).
    transfers: Vec<ShellTransfer>,
    actions: Vec<&'static str>,
}

/// One file moving now: `Devices[].transfers`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ShellTransfer {
    name: String,
    incoming: bool,
    /// 0..=1000; 0 while still preparing.
    permille: u32,
    paused: bool,
    /// "fast lane" or "over the secure link"; "" not said yet.
    lane: String,
}

fn shell_transfers(device_id: &str) -> Vec<ShellTransfer> {
    use crate::device_links::webrtc::transfer_board;
    transfer_board::rows()
        .into_iter()
        .filter(|row| row.device_id == device_id)
        .map(|row| ShellTransfer {
            permille: if row.total == 0 || row.preparing {
                0
            } else {
                (row.done.min(row.total) * 1000 / row.total) as u32
            },
            lane: transfer_board::lane_of(&row.id)
                .unwrap_or_default()
                .to_string(),
            name: row.name,
            incoming: row.incoming,
            paused: row.paused,
        })
        .take(RECENT_TRANSFERS)
        .collect()
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
        settings: SWITCHES
            .iter()
            .filter(|spec| spec.scope == Scope::Phone)
            .filter_map(|spec| Some((spec.key, phone_switch(spec.key, device, settings)?)))
            .collect(),
        sound: {
            let sound = device.sound.clone().unwrap_or_default();
            [
                sound.sending.is_some(),
                sound.hearing.as_deref() == Some("mic"),
                sound.hearing.as_deref() == Some("system"),
            ]
        },
        rtt_ms: device.rtt_ms.map_or(-1, |ms| ms as i32),
        unread: if settings.shows_notifications(&device.id) {
            device.notifications.len() as u32
        } else {
            0
        },
        text_capture: crate::device_links::webrtc::capture::text_available(&device.id),
        transfers: if device.feature_transport_ready {
            shell_transfers(&device.id)
        } else {
            Vec::new()
        },
        actions: actions(device, settings),
    }
}

/// What the phone takes from this computer: the window's own test, so a tile
/// is offered exactly when the window would offer its button.
fn takes(device: &DeviceView, packet_type: &str) -> bool {
    crate::window::can_send_feature(device, packet_type)
}

/// The Insert-from-phone actions and their rows in `window::insert::CAPTURES`.
const INSERTS: [(&str, usize); 5] = [
    ("insert-photo", 0),
    ("insert-photo-file", 1),
    ("insert-scan", 2),
    ("insert-text", 3),
    ("insert-qr", 4),
];

/// The pages of the window a client may open for a phone, by action.
const PAGES: [(&str, &str); 8] = [
    ("open-files", "files"),
    ("open-notes", "notes"),
    ("open-clipboard", "clipboard"),
    ("open-snippets", "snippets"),
    ("open-notifications", "notifications"),
    ("open-contacts", "contacts"),
    ("open-terminals", "terminals"),
    ("open-settings", "settings"),
];

/// What a client may offer for `device` right now.
fn actions(device: &DeviceView, settings: &AppSettings) -> Vec<&'static str> {
    use crate::device_links::packet::{
        PACKET_TYPE_CLIPBOARD, PACKET_TYPE_FINDMYPHONE_REQUEST, PACKET_TYPE_LOCK_REQUEST,
        PACKET_TYPE_MOUSEPAD_REQUEST, PACKET_TYPE_PING, PACKET_TYPE_SHARE_REQUEST,
    };
    let mut actions = vec!["open"];
    if device.feature_transport_ready {
        for (action, packet) in [
            ("ring", PACKET_TYPE_FINDMYPHONE_REQUEST),
            ("lock", PACKET_TYPE_LOCK_REQUEST),
            ("send-files", PACKET_TYPE_SHARE_REQUEST),
            ("send-link", PACKET_TYPE_SHARE_REQUEST),
            ("send-clipboard", PACKET_TYPE_CLIPBOARD),
            ("ping", PACKET_TYPE_PING),
            ("show-screen", PACKET_TYPE_MOUSEPAD_REQUEST),
        ] {
            if takes(device, packet) {
                actions.push(action);
            }
        }
        // A shared terminal starts at this computer; it needs nothing of the phone.
        actions.push("new-terminal");
        // A photo, a scan, text or a QR code from the phone (F3, C2): the
        // window offers them on the same test; "text" also needs its OCR.
        if takes(device, PACKET_TYPE_MOUSEPAD_REQUEST) {
            for (action, _) in INSERTS {
                if action != "insert-text"
                    || crate::device_links::webrtc::capture::text_available(&device.id)
                {
                    actions.push(action);
                }
            }
        }
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
        actions.extend(["media-prev", "media-play-pause", "media-next"]);
    }
    // The window's own pages and its Unpair question, for a paired phone.
    if device.trusted {
        actions.extend(PAGES.iter().map(|(action, _)| *action));
        actions.push("unpair");
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

/// Whose switch it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scope {
    Phone,
    Desk,
}

/// A switch a client may set with `SetSwitch`. `sensitive` ones are off by
/// default for a security reason (they let a phone run commands, unlock or
/// fill the disk, or let a phone hear this room); they can only be turned
/// OFF from the top bar. Turning one on is DeskLink's own page.
struct SwitchSpec {
    key: &'static str,
    scope: Scope,
    sensitive: bool,
}

const fn switch(key: &'static str, scope: Scope, sensitive: bool) -> SwitchSpec {
    SwitchSpec {
        key,
        scope,
        sensitive,
    }
}

const SWITCHES: [SwitchSpec; 18] = [
    switch("showNotifications", Scope::Phone, false),
    switch("dndShare", Scope::Phone, false),
    switch("commandAlerts", Scope::Phone, false),
    switch("namesCallers", Scope::Phone, false),
    switch("walkAway", Scope::Phone, false),
    switch("terminalAccess", Scope::Phone, true),
    switch("unlockNearby", Scope::Phone, true),
    switch("galleryAccept", Scope::Phone, true),
    switch("phoneKeepAwake", Scope::Phone, true),
    switch("glide", Scope::Phone, true),
    switch("deskSound", Scope::Phone, false),
    switch("phoneMic", Scope::Phone, false),
    switch("phoneSound", Scope::Phone, false),
    switch("keepAwake", Scope::Desk, false),
    switch("clipboard", Scope::Desk, false),
    switch("forwardNotifications", Scope::Desk, false),
    switch("pauseMediaOnCall", Scope::Desk, false),
    switch("deskMic", Scope::Desk, true),
];

/// A phone's switch as published (`None`: not a state switch; sound is in
/// `Devices[].sound`).
fn phone_switch(key: &str, device: &DeviceView, settings: &AppSettings) -> Option<bool> {
    let id = device.id.as_str();
    Some(match key {
        "showNotifications" => settings.shows_notifications(id),
        "dndShare" => settings.shares_dnd_with(id),
        "commandAlerts" => settings.alerts_commands_to(id),
        "namesCallers" => settings.names_callers_from(id),
        "walkAway" => settings.locks_when_away(id),
        "terminalAccess" => device.terminal_access,
        "unlockNearby" => settings.unlocks_from(id),
        "galleryAccept" => settings.accepts_gallery_from(id),
        "phoneKeepAwake" => settings.lets_keep_awake(id),
        "glide" => settings
            .glide
            .as_ref()
            .is_some_and(|glide| glide.device == id),
        _ => return None,
    })
}

/// The command a switch comes to, or why not. Pure: the one table behind
/// `SetSwitch`. A sensitive switch only goes off here.
fn resolve_switch(key: &str, device_id: &str, on: bool) -> Result<DaemonCommand, String> {
    let spec = SWITCHES
        .iter()
        .find(|spec| spec.key == key)
        .ok_or_else(|| format!("\"{key}\" is not a switch"))?;
    match (spec.scope, device_id.is_empty()) {
        (Scope::Phone, true) => return Err(format!("\"{key}\" needs a device")),
        (Scope::Desk, false) => return Err(format!("\"{key}\" belongs to this computer")),
        _ => {}
    }
    if spec.sensitive && on {
        return Err(format!(
            "\"{key}\" is turned on in DeskLink's own settings, not from the top bar"
        ));
    }
    let id = device_id.to_string();
    Ok(match key {
        "showNotifications" => DaemonCommand::SetShowNotifications(id, on),
        "dndShare" => DaemonCommand::SetDndShare(id, on),
        "commandAlerts" => DaemonCommand::SetCommandAlerts(id, on),
        "namesCallers" => DaemonCommand::SetContactCallerNames(id, on),
        "walkAway" => DaemonCommand::SetWalkAwayLock(id, on),
        "terminalAccess" => DaemonCommand::SetTerminalAccess(id, on),
        "unlockNearby" => DaemonCommand::SetUnlockNearby(id, on),
        "galleryAccept" => DaemonCommand::SetGalleryAccept(id, on),
        "phoneKeepAwake" => DaemonCommand::SetKeepAwakeFrom(id, on),
        "glide" => DaemonCommand::SetGlide(id, None),
        "deskSound" => DaemonCommand::Sound(id, "desk-sound".into(), on),
        "phoneMic" => DaemonCommand::Sound(id, "phone-mic".into(), on),
        "phoneSound" => DaemonCommand::Sound(id, "phone-sound".into(), on),
        "keepAwake" => DaemonCommand::SetStayAwake(on),
        "clipboard" => DaemonCommand::SetClipboardSync(on),
        "forwardNotifications" => DaemonCommand::SetForwardNotifications(on),
        "pauseMediaOnCall" => DaemonCommand::SetPauseMediaOnCall(on),
        "deskMic" => DaemonCommand::SetDeskMicToPhone(on),
        _ => return Err(format!("\"{key}\" is not a switch")),
    })
}

/// Whether `SetSwitch` may run `key` for `device` now: the phone is paired,
/// sound needs a live link, sharing Do Not Disturb needs GNOME's settings.
fn switch_allowed(device: Option<&ShellDevice>, key: &str, on: bool) -> Result<(), String> {
    let Some(device) = device else {
        return Ok(());
    };
    if !device.paired {
        return Err("that phone is not paired".to_string());
    }
    if matches!(key, "deskSound" | "phoneMic" | "phoneSound") && on && !device.ready {
        return Err(format!("{} is not connected", device.name));
    }
    if key == "dndShare" && on && crate::device_links::dnd::desk_state().is_none() {
        return Err("Do Not Disturb cannot be shared on this desktop".to_string());
    }
    Ok(())
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
        let switches = glib::VariantDict::new(None);
        for (key, on) in &self.settings {
            switches.insert(key, *on);
        }
        dict.insert_value("settings", &switches.end());
        let sound = glib::VariantDict::new(None);
        sound.insert("deskSound", self.sound[0]);
        sound.insert("phoneMic", self.sound[1]);
        sound.insert("phoneSound", self.sound[2]);
        dict.insert_value("sound", &sound.end());
        dict.insert("rttMs", self.rtt_ms);
        dict.insert("unread", self.unread);
        dict.insert("textCapture", self.text_capture);
        let transfers = self.transfers.iter().map(|transfer| {
            let entry = glib::VariantDict::new(None);
            entry.insert("name", &transfer.name);
            entry.insert("incoming", transfer.incoming);
            entry.insert("fraction", f64::from(transfer.permille) / 1000.0);
            entry.insert("paused", transfer.paused);
            entry.insert("lane", &transfer.lane);
            entry.end()
        });
        dict.insert_value(
            "transfers",
            &Variant::array_from_iter_with_type(glib::VariantTy::VARDICT, transfers),
        );
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

/// The phones' ongoing items (W13) that show now, for the top-bar chip. `base`
/// is on THIS computer's clock (milliseconds since the epoch), -1 for none,
/// so the extension needs no offset of its own.
fn activities_variant(devices: &[DeviceView], settings: &AppSettings, now_ms: i64) -> Variant {
    let dicts = devices
        .iter()
        .filter(|device| device.trusted)
        .flat_map(|device| {
            let offset = device.activity.as_ref().map_or(0, |a| a.clock_offset_ms);
            crate::device_links::activity::shown(
                device.activity.as_ref(),
                device.feature_transport_ready,
                settings.shows_notifications(&device.id),
                now_ms,
            )
            .into_iter()
            .map(move |item| {
                let dict = glib::VariantDict::new(None);
                dict.insert("device", &device.id);
                dict.insert("deviceName", &device.name);
                dict.insert("id", &item.id);
                dict.insert("app", &item.app_label);
                dict.insert("title", &item.title);
                dict.insert("kind", &item.kind);
                dict.insert("progress", i64::from(item.progress));
                dict.insert(
                    "base",
                    if item.chronometer_base > 0 {
                        item.chronometer_base.saturating_add(offset)
                    } else {
                        -1
                    },
                );
                dict.insert("countdown", item.countdown);
                dict.end()
            })
            .collect::<Vec<_>>()
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
    let announced = Rc::clone(&properties);
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
                        let check = check_hello_sender(&connection, sender);
                        if check == Check::Shell {
                            client_said_hello(&connection, sender, vetted(capabilities));
                            announce_to(&connection, sender, &announced.borrow());
                        } else if check == Check::Throttled {
                            // Someone else's question spent the window; the
                            // Shell says Hello once, so it is tried again
                            // just after it, not refused.
                            let (connection, sender) = (connection.clone(), sender.to_string());
                            let capabilities = vetted(capabilities);
                            let announced = Rc::clone(&announced);
                            gtk::glib::timeout_add_local_once(
                                OWNER_ASK_EVERY + std::time::Duration::from_millis(100),
                                move || {
                                    if hello_is_from_the_shell(&connection, &sender) {
                                        client_said_hello(&connection, &sender, capabilities);
                                        announce_to(&connection, &sender, &announced.borrow());
                                    }
                                },
                            );
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
                    // Every verb has an outward effect (ring or lock the phone,
                    // send the clipboard, change what is shared): only the
                    // Shell, which the extension runs inside, may ask.
                    if !is_the_shell_caller(&connection, sender) {
                        return invocation.return_dbus_error(
                            "org.freedesktop.DBus.Error.AccessDenied",
                            "Only GNOME Shell may use the top-bar interface",
                        );
                    }
                    match call(&app, method, &params) {
                        Ok(()) => invocation.return_value(None),
                        Err(error) => invocation.return_dbus_error(INVALID_ARGS, &error),
                    }
                })
            },
        )
        .property(move |connection, sender, _, _, name| {
            // Phone names, call numbers, notifications, snippets and what is
            // allowed per phone are for the Shell only: anyone else on the
            // session bus reads empty values (the version is not private).
            let from_shell = name == "Version" || is_the_shell_caller(&connection, sender);
            visible_property(name, &shown.borrow(), from_shell)
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
            // Sent to the Shell alone: a broadcast would hand the same values
            // to any process that listens on the session bus.
            if !changed.is_empty() {
                if let Some(owner) = known_shell_owner() {
                    emit_changed(&connection, &owner, changed);
                }
            }
            glib::ControlFlow::Continue
        })
    });

    offer_extension(app);
}

/// The empty value of a property, of its declared type.
fn empty_property(name: &str) -> Variant {
    match name {
        "Desk" => glib::VariantDict::new(None).end(),
        _ => Variant::array_from_iter_with_type(
            glib::VariantTy::VARDICT,
            std::iter::empty::<Variant>(),
        ),
    }
}

/// What a reader of property `name` gets: the real value for the Shell, an
/// empty one of the same type for anyone else.
fn visible_property(
    name: &str,
    values: &std::collections::HashMap<&'static str, Variant>,
    from_shell: bool,
) -> Variant {
    match values.get(name) {
        Some(value) if from_shell || name == "Version" => value.clone(),
        Some(_) => empty_property(name),
        None => VERSION.to_variant(),
    }
}

/// `PropertiesChanged` to one bus name (the Shell's), never a broadcast.
fn emit_changed(
    connection: &gio::DBusConnection,
    destination: &str,
    changed: std::collections::HashMap<String, Variant>,
) {
    let changed = (INTERFACE, changed, Vec::<String>::new()).to_variant();
    let _ = connection.emit_signal(
        Some(destination),
        PATH,
        "org.freedesktop.DBus.Properties",
        "PropertiesChanged",
        Some(&changed),
    );
}

/// Every current value, to a client that just proved it is the Shell (its
/// first read may have come before the owner was known).
fn announce_to(
    connection: &gio::DBusConnection,
    shell: &str,
    values: &std::collections::HashMap<&'static str, Variant>,
) {
    emit_changed(
        connection,
        shell,
        values
            .iter()
            .map(|(name, value)| (name.to_string(), value.clone()))
            .collect(),
    );
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
        (
            "Activities",
            activities_variant(&views, &settings, crate::device_links::activity::now_ms()),
        ),
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
        "SetSwitch" => {
            let (device_id, key, on) = params.get::<(String, String, bool)>().unwrap_or_default();
            let command = resolve_switch(&key, &device_id, on)?;
            let devices = current(app);
            let device = devices.iter().find(|device| device.id == device_id);
            if !device_id.is_empty() && device.is_none() {
                return Err(format!("no device \"{device_id}\""));
            }
            switch_allowed(device, &key, on)?;
            daemon.send(command);
            Ok(())
        }
        "SendLink" => {
            let (device_id, link) = params.get::<(String, String)>().unwrap_or_default();
            let devices = current(app);
            check(&devices, &device_id, "send-link")?;
            let link = crate::window::link_to_send(&link).ok_or("not a link")?;
            daemon.send(DaemonCommand::SendShareText(device_id, link));
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
        ("ping", Some(id), Some(daemon)) => daemon.send(DaemonCommand::SendPing(id)),
        ("media-prev" | "media-play-pause" | "media-next", Some(id), Some(daemon)) => {
            let player = daemon
                .devices()
                .into_iter()
                .find(|known| known.id == id)
                .and_then(|known| known.media_status)
                .map(|media| media.player);
            if let Some(player) = player {
                let verb = match action {
                    "media-next" => "Next",
                    "media-prev" => "Previous",
                    _ => "PlayPause",
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
        (insert, Some(id), Some(daemon)) if insert.starts_with("insert-") => {
            let capture = INSERTS
                .iter()
                .find(|(action, _)| *action == insert)
                .and_then(|(_, index)| crate::window::insert::CAPTURES.get(*index));
            if let Some((kind, target, ..)) = capture {
                daemon.send(DaemonCommand::Capture(id, (*kind).into(), (*target).into()));
            }
        }
        ("show-screen", Some(id), Some(daemon)) => {
            // The phone still has to agree to share; the dialog says so.
            if let Some(window) = main_window(app) {
                crate::remote_control::show_remote_control_dialog(daemon, id, &window);
            }
        }
        ("unpair", Some(id), Some(daemon)) => {
            // The window's own question, never a silent unpairing.
            if let Some(window) = main_window(app).and_downcast::<crate::DesklinkWindow>() {
                let name = device.map(|device| device.name.clone()).unwrap_or_default();
                window.show_device(&id);
                crate::window::confirm(
                    &window,
                    &format!("Unpair {name}?"),
                    "It can connect again only after pairing again.",
                    "_Unpair",
                    move || daemon.send(DaemonCommand::Unpair(id.clone())),
                );
            }
        }
        (page, Some(id), _) if PAGES.iter().any(|(action, _)| *action == page) => {
            if let (Some(window), Some((_, name))) = (
                main_window(app).and_downcast::<crate::DesklinkWindow>(),
                PAGES.iter().find(|(action, _)| *action == page),
            ) {
                window.show_device(&id);
                window.show_page(name);
            }
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

/// Whether a method call came from the Shell. The one gate for every
/// top-bar verb; a test pins that no verb is reachable around it.
fn is_the_shell_caller(connection: &gio::DBusConnection, sender: Option<&str>) -> bool {
    sender.is_some_and(|sender| hello_is_from_the_shell(connection, sender))
}

/// The Shell's bus name as last asked for, and when. A bus name is never reused,
/// so a cached owner that matches the caller is still right after any restart;
/// only a caller that does not match needs a fresh question.
#[derive(Default)]
struct OwnerCache {
    asked: Option<std::time::Instant>,
    owner: Option<String>,
}

/// A fresh question about the owner is asked at most this often: the call blocks
/// the main loop (up to its 500 ms timeout), and any process of the same user may
/// call these verbs in a loop.
const OWNER_ASK_EVERY: std::time::Duration = std::time::Duration::from_secs(2);

/// What the owner check came to.
#[derive(Debug, PartialEq, Eq)]
enum Check {
    Shell,
    NotTheShell,
    /// Not asked: a question was asked within [`OWNER_ASK_EVERY`] (by anyone).
    /// Not an answer: a one-time `Hello` is tried again after the window.
    Throttled,
}

fn check_the_shell(
    cache: &mut OwnerCache,
    now: std::time::Instant,
    sender: &str,
    ask: impl FnOnce() -> Option<String>,
) -> Check {
    if is_the_shell(sender, cache.owner.as_deref()) {
        return Check::Shell;
    }
    if cache
        .asked
        .is_some_and(|asked| now.duration_since(asked) < OWNER_ASK_EVERY)
    {
        return Check::Throttled;
    }
    cache.asked = Some(now);
    cache.owner = ask();
    if is_the_shell(sender, cache.owner.as_deref()) {
        Check::Shell
    } else {
        Check::NotTheShell
    }
}

#[cfg(test)]
fn is_the_shell_cached(
    cache: &mut OwnerCache,
    now: std::time::Instant,
    sender: &str,
    ask: impl FnOnce() -> Option<String>,
) -> bool {
    check_the_shell(cache, now, sender, ask) == Check::Shell
}

fn hello_is_from_the_shell(connection: &gio::DBusConnection, sender: &str) -> bool {
    check_hello_sender(connection, sender) == Check::Shell
}

thread_local! {
    static OWNER: std::cell::RefCell<OwnerCache> = std::cell::RefCell::new(OwnerCache::default());
}

/// The Shell's bus name, once it has been asked for and answered.
fn known_shell_owner() -> Option<String> {
    OWNER.with(|cache| cache.borrow().owner.clone())
}

fn check_hello_sender(connection: &gio::DBusConnection, sender: &str) -> Check {
    OWNER.with(|cache| {
        check_the_shell(
            &mut cache.borrow_mut(),
            std::time::Instant::now(),
            sender,
            || {
                connection
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
                    .map(|(owner,)| owner)
            },
        )
    })
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
    let state = EXTENSION_KNOWN
        .with(|known| known.get())
        .then(|| EXTENSION_IS_ACTIVE.with(|active| active.get()));
    CLIENT.with(|client| {
        client
            .borrow()
            .as_ref()
            .is_some_and(|client| handles(&client.capabilities, capability, state))
    })
}

/// A client said it shows `capability`; but the extension may have been
/// turned off or crashed since (the bus name it said Hello from is the
/// Shell's, which stays), so a state known to be "not active" wins (I4).
fn handles(capabilities: &[String], capability: &str, extension_active: Option<bool>) -> bool {
    extension_active != Some(false) && capabilities.iter().any(|known| known == capability)
}

/// Whether DeskLink's top-bar extension is running, so it already shows
/// sessions (kept current from `ExtensionStateChanged`, not polled).
pub fn extension_active() -> bool {
    EXTENSION_IS_ACTIVE.with(|active| active.get())
}

/// The extension, built into the app so any install can turn it on.
const EXTENSION_FILES: [(&str, &str); 7] = [
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
    (
        "model.js",
        include_str!("../gnome-shell-extension/desklink@derx06.github.io/model.js"),
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
const KNOWN_BUILDS: [&str; 17] = [
    // The current build (links checked, focus kept only while open).
    "32a1f784c930664df85dfce8b6572ea1a8608150585320cfb6c7fb090ac854a7",
    // The complete remote, first cut (model.js, sections, switches).
    "b1c3aeb335ad359842ef42bbc1951fe68acc047c5be6116aea83185e2ae1f737",
    // Wave 2: the ongoing-items chip in the pill.
    "c27272d96fac1f6029a7494eacbe93b716a44b05959acb0ded9b697e8f42a5d2",
    // w8: the Photo tile, `insert-photo`.
    "4d5b8192ae0a8fdcbceef31bcf955ff08d555b8cae633d758d77521e376687e4",
    // w7c: no images decoded in the shell, follow-up messages announced,
    // the reply dialog destroyed, accent fallback.
    "23025d05bb0300675bc3d5f1c046be8b267deb7f6692767003822244d1341725",
    // W4: the Files row, Settings in its comments.
    "b07fccfbb89268d72553923fe74ed53089e2042fac1fe006ede1f89ff7fd2e4e",
    // The Bucket's sync progress row.
    "9bdbd7405e48738c94bd65dfae6baa2f412c78c9f892fad0bfe5c2656858138f",
    // 35a2ff3, F4: glide in the live row.
    "1744f9ec921dbf1f1e3fb9c4215b87190518f4dbe40cd8ce21a5336150987d3f",
    // F6: Snippets and New Note in the menu.
    "d20b1b57eea99e3c781feb69b6ab5bed3393e22e0eb81d88e5b7d30af5cb88e4",
    // Top-bar redesign: menu.js, the glyph icon.
    "3413a34defbe375ac5347683de5a53f89d0df1c11e586b9b454cc43348154e77",
    // be1b760, 2026-10-21.
    "07555cf126179911873994980fc0a360b4fea38819bee64277a44ad752ab2649",
    // 9784a7d, 2026-10-21.
    "2aa899e5394ac50cb6fbbb536d135797d93bf5d8adcfa50ba130c743d956154b",
    // f798285, 2026-10-21.
    "cc953aaa3a1db690eff0f36053be389f6527da1921f1285829a0e6ced2f7e06f",
    // 1b78f08, 2026-10-21.
    "aa5bffbecce7a7ccc59b6a0e611dbd44ece3377a1ed547305266d941079d476e",
    // ba03b84, 2026-10-21.
    "d68895988634a5a62377c63ca97bd643f901f232419b4b8c84f341b20c49f4f4",
    // 635fc5d, 2026-10-20.
    "cff05ba7d6f7e20074ae6fa7406177e58ac4bf8901d07faace5f131f07da8878",
    // 1f8c0a4, 2026-10-20.
    "1fedcbddc921c5b1a5ebe46fd73127d6a732c0f67d74e78f24fa06402abbafe9",
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

/// `enabled-extensions` without ours; every other extension stays.
fn without_extension(enabled: Vec<String>) -> Vec<String> {
    enabled
        .into_iter()
        .filter(|uuid| uuid != EXTENSION_UUID)
        .collect()
}

/// "Turn off": disables only DeskLink's extension. The system-wide
/// `disable-user-extensions` switch that "Turn On" may have flipped is left
/// as it is (other extensions may depend on it now).
pub fn turn_off_extension() -> Result<(), String> {
    let settings = gio::Settings::new("org.gnome.shell");
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
            without_extension(strings("enabled-extensions")),
        )
        .map_err(|error| error.to_string())?;
    let mut disabled = strings("disabled-extensions");
    if !disabled.iter().any(|uuid| uuid == EXTENSION_UUID) {
        disabled.push(EXTENSION_UUID.to_string());
    }
    settings
        .set_strv("disabled-extensions", disabled)
        .map_err(|error| error.to_string())?;
    gio::Settings::sync();
    Ok(())
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

    /// W13: the top bar gets a phone's shown items with `base` on this
    /// computer's clock; nothing while it is not ready or its notifications
    /// are hidden.
    #[test]
    fn the_top_bar_gets_the_ongoing_items_on_this_computers_clock() {
        let info = crate::device_links::device_info::DeviceInfo::local("p".into(), "Pixel".into());
        let mut device = DeviceView::from_info(&info, "10.0.0.2".into(), true);
        device.trusted = true;
        device.feature_transport_ready = true;
        let (phone_at, now) = (5_000_000_i64, 9_000_000_i64);
        let mut body = serde_json::Map::new();
        body.insert("at".into(), serde_json::json!(phone_at));
        body.insert(
            "items".into(),
            serde_json::json!([{"id": "t", "appLabel": "Clock", "title": "Pasta", "kind": "timer",
                "chronometerBase": phone_at + 60_000, "countdown": true}]),
        );
        device.activity = Some(crate::device_links::activity::snapshot_from(&body, now));
        let settings = AppSettings::default();
        let one = activities_variant(std::slice::from_ref(&device), &settings, now);
        assert_eq!(one.n_children(), 1);
        let item = one.child_value(0);
        let dict = glib::VariantDict::new(Some(&item));
        let get = |key: &str| dict.lookup_value(key, None).unwrap();
        assert_eq!(get("title").str(), Some("Pasta"));
        assert_eq!(get("base").get::<i64>(), Some(now + 60_000), "this clock");
        assert_eq!(get("countdown").get::<bool>(), Some(true));
        device.feature_transport_ready = false;
        assert_eq!(
            activities_variant(std::slice::from_ref(&device), &settings, now).n_children(),
            0
        );
        device.feature_transport_ready = true;
        let mut hidden = settings;
        hidden.hidden_notifications.insert("p".into());
        assert_eq!(activities_variant(&[device], &hidden, now).n_children(), 0);
    }

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
            app_version: None,
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
        // The phone takes nothing yet (no capabilities): only what is local.
        assert_eq!(
            dict.lookup::<Vec<String>>("actions").unwrap().unwrap(),
            [
                "open",
                "new-terminal",
                "open-files",
                "open-notes",
                "open-clipboard",
                "open-snippets",
                "open-notifications",
                "open-contacts",
                "open-terminals",
                "open-settings",
                "unpair",
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
        device.trusted = false;
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
                .contains(&"cff05ba7d6f7e20074ae6fa7406177e58ac4bf8901d07faace5f131f07da8878"),
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
        // Turning off removes only ours.
        assert_eq!(without_extension(enabled), vec!["other@x".to_string()]);
    }

    #[test]
    fn a_disabled_extension_no_longer_handles_notifications() {
        let caps = vec!["notifications".to_string()];
        assert!(handles(&caps, "notifications", Some(true)));
        assert!(handles(&caps, "notifications", None), "state not known yet");
        assert!(!handles(&caps, "notifications", Some(false)), "turned off");
        assert!(!handles(&caps, "calls", Some(true)), "never said so");
    }

    /// Every top-bar verb passes the sender check first (no path around it).
    #[test]
    fn the_sender_is_checked_before_any_verb_runs() {
        let source = include_str!("shell_dbus.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap();
        let gate = source
            .find("is_the_shell_caller(&connection, sender)")
            .unwrap();
        let run = source.find("match call(&app, method, &params)").unwrap();
        assert!(gate < run, "the gate comes before the verbs");
        assert_eq!(
            source.matches("match call(&app, method, &params)").count(),
            1,
            "and there is one way in"
        );
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

    /// A stranger's question spends the window: the Shell's one Hello inside
    /// it is "throttled" (and tried again after it), not "not the Shell".
    #[test]
    fn a_hello_inside_the_window_is_retried_not_refused() {
        let mut cache = OwnerCache::default();
        let start = std::time::Instant::now();
        assert_eq!(
            check_the_shell(&mut cache, start, ":1.99", || Some(":1.7".into())),
            Check::NotTheShell
        );
        // The Shell restarted a moment later with a new name.
        assert_eq!(
            check_the_shell(&mut cache, start, ":1.8", || Some(":1.8".into())),
            Check::Throttled
        );
        let after = start + OWNER_ASK_EVERY + std::time::Duration::from_millis(100);
        assert_eq!(
            check_the_shell(&mut cache, after, ":1.8", || Some(":1.8".into())),
            Check::Shell
        );
    }

    #[test]
    fn a_flood_of_callers_costs_one_bus_question_per_window() {
        let mut cache = OwnerCache::default();
        let start = std::time::Instant::now();
        let asked = std::cell::Cell::new(0);
        let ask = |owner: &'static str| {
            let asked = &asked;
            move || {
                asked.set(asked.get() + 1);
                Some(owner.to_string())
            }
        };
        // The Shell is asked once, then answered from memory.
        assert!(is_the_shell_cached(&mut cache, start, ":1.7", ask(":1.7")));
        assert!(is_the_shell_cached(&mut cache, start, ":1.7", ask(":1.7")));
        assert_eq!(asked.get(), 1);
        // A stranger repeating itself asks nothing more inside the window.
        for _ in 0..1000 {
            assert!(!is_the_shell_cached(
                &mut cache,
                start,
                ":1.99",
                ask(":1.7")
            ));
        }
        assert_eq!(asked.get(), 1);
        // After it, one fresh question: a restarted Shell has a new name.
        let later = start + OWNER_ASK_EVERY;
        assert!(is_the_shell_cached(&mut cache, later, ":1.8", ask(":1.8")));
        assert_eq!(asked.get(), 2);
        assert!(
            !is_the_shell_cached(&mut cache, later, ":1.7", ask(":1.8")),
            "the old name is gone"
        );
    }

    /// A phone that takes everything the window offers.
    fn capable() -> DeviceView {
        use crate::device_links::packet::*;
        let mut device = phone();
        device.status = DeviceStatus::Paired;
        device.trusted = true;
        device.feature_transport_ready = true;
        device.incoming_capabilities = [
            PACKET_TYPE_FINDMYPHONE_REQUEST,
            PACKET_TYPE_LOCK_REQUEST,
            PACKET_TYPE_SHARE_REQUEST,
            PACKET_TYPE_CLIPBOARD,
            PACKET_TYPE_PING,
            PACKET_TYPE_MOUSEPAD_REQUEST,
        ]
        .map(String::from)
        .to_vec();
        device
    }

    #[test]
    fn the_remote_offers_each_action_only_when_the_phone_takes_it() {
        let device = capable();
        let all = actions(&device, &AppSettings::default());
        for action in [
            "ring",
            "lock",
            "send-files",
            "send-link",
            "send-clipboard",
            "ping",
            "show-screen",
            "new-terminal",
            "insert-photo",
            "insert-photo-file",
            "insert-scan",
            "insert-qr",
            "open-files",
            "open-notes",
            "open-clipboard",
            "open-snippets",
            "open-notifications",
            "open-contacts",
            "open-terminals",
            "open-settings",
            "unpair",
        ] {
            assert!(all.contains(&action), "{action}");
        }
        assert!(!all.contains(&"media-prev"), "no music");
        // A phone that takes no ping, no screen and no pictures is not offered them.
        let mut bare = capable();
        bare.incoming_capabilities = vec![crate::device_links::packet::PACKET_TYPE_PING.into()];
        let few = actions(&bare, &AppSettings::default());
        assert!(few.contains(&"ping"));
        for action in ["ring", "lock", "send-files", "show-screen", "insert-scan"] {
            assert!(!few.contains(&action), "{action}");
        }
        // Not paired: no pages, no unpair.
        let mut stranger = capable();
        stranger.trusted = false;
        let list = actions(&stranger, &AppSettings::default());
        assert!(!list.contains(&"unpair") && !list.contains(&"open-notes"));
        // Not ready: nothing that needs the phone.
        let mut offline = capable();
        offline.feature_transport_ready = false;
        let list = actions(&offline, &AppSettings::default());
        assert!(!list.contains(&"ring") && !list.contains(&"insert-qr"));
        // Every action is allowed through `check`, and only for its device.
        let devices = [shell_device(&device, &AppSettings::default())];
        assert!(check(&devices, "phone1", "ping").is_ok());
        assert!(check(&devices, "", "ping").is_err());
        assert!(check(&devices, "phone1", "send-link").is_ok());
    }

    #[test]
    fn music_gets_a_previous_track_too() {
        let mut device = capable();
        device.media_status = Some(crate::device_links::device::MediaStatus {
            player: "Spotify".into(),
            player_list: vec!["Spotify".into()],
            title: "Song".into(),
            artist: "Band".into(),
            album: String::new(),
            is_playing: true,
            volume: None,
            position: None,
            length: None,
            can_seek: false,
            album_art_url: None,
            album_art: None,
        });
        let list = actions(&device, &AppSettings::default());
        for action in ["media-prev", "media-play-pause", "media-next"] {
            assert!(list.contains(&action), "{action}");
        }
    }

    /// The one table behind `SetSwitch`: every key resolves, a switch that is
    /// off for security never turns on from the top bar, and the scope is kept.
    #[test]
    fn a_switch_resolves_to_its_command_and_security_ones_only_go_off() {
        for spec in &SWITCHES {
            let id = if spec.scope == Scope::Phone { "p" } else { "" };
            let off = resolve_switch(spec.key, id, false);
            assert!(off.is_ok(), "{} off", spec.key);
            let on = resolve_switch(spec.key, id, true);
            assert_eq!(on.is_err(), spec.sensitive, "{} on", spec.key);
            // The wrong scope never resolves.
            let wrong = if spec.scope == Scope::Phone { "" } else { "p" };
            assert!(
                resolve_switch(spec.key, wrong, false).is_err(),
                "{}",
                spec.key
            );
        }
        assert!(matches!(
            resolve_switch("showNotifications", "p", false),
            Ok(DaemonCommand::SetShowNotifications(id, false)) if id == "p"
        ));
        assert!(matches!(
            resolve_switch("terminalAccess", "p", false),
            Ok(DaemonCommand::SetTerminalAccess(id, false)) if id == "p"
        ));
        assert!(matches!(
            resolve_switch("phoneMic", "p", true),
            Ok(DaemonCommand::Sound(id, what, true)) if id == "p" && what == "phone-mic"
        ));
        assert!(matches!(
            resolve_switch("keepAwake", "", true),
            Ok(DaemonCommand::SetStayAwake(true))
        ));
        assert!(matches!(
            resolve_switch("glide", "p", false),
            Ok(DaemonCommand::SetGlide(id, None)) if id == "p"
        ));
        for key in [
            "terminalAccess",
            "unlockNearby",
            "galleryAccept",
            "phoneKeepAwake",
            "glide",
        ] {
            assert!(resolve_switch(key, "p", true).is_err(), "{key}");
        }
        assert!(resolve_switch("deskMic", "", true).is_err());
        assert!(resolve_switch("explode", "p", true).is_err());
        assert!(
            resolve_switch("walkAway", "", true).is_err(),
            "needs a phone"
        );
    }

    #[test]
    fn a_switch_needs_a_paired_phone_and_sound_a_live_link() {
        let mut device = capable();
        let mapped = shell_device(&device, &AppSettings::default());
        assert!(switch_allowed(Some(&mapped), "deskSound", true).is_ok());
        assert!(switch_allowed(None, "keepAwake", true).is_ok());
        device.feature_transport_ready = false;
        let offline = shell_device(&device, &AppSettings::default());
        assert!(switch_allowed(Some(&offline), "deskSound", true).is_err());
        assert!(
            switch_allowed(Some(&offline), "deskSound", false).is_ok(),
            "off is always possible"
        );
        device.trusted = false;
        let stranger = shell_device(&device, &AppSettings::default());
        assert!(switch_allowed(Some(&stranger), "commandAlerts", false).is_err());
    }

    #[test]
    fn the_settings_sound_and_counts_are_published() {
        let mut settings = AppSettings::default();
        settings.hidden_notifications.insert("phone1".into());
        settings.dnd_devices.insert("phone1".into());
        settings.unlock_devices.insert("phone1".into());
        let mut device = capable();
        device.terminal_access = true;
        device.rtt_ms = Some(12);
        device.sound = Some(crate::device_links::device::SoundUse {
            sending: Some("system".into()),
            hearing: Some("mic".into()),
            hearing_live: true,
        });
        let mapped = shell_device(&device, &settings);
        let list = devices_variant(&[mapped]);
        let dict = glib::VariantDict::new(Some(&list.child_value(0)));
        let switches = glib::VariantDict::new(dict.lookup_value("settings", None).as_ref());
        let flag = |key: &str| switches.lookup::<bool>(key).unwrap();
        assert_eq!(flag("showNotifications"), Some(false), "hidden here");
        assert_eq!(flag("dndShare"), Some(true));
        assert_eq!(flag("unlockNearby"), Some(true));
        assert_eq!(flag("terminalAccess"), Some(true));
        assert_eq!(flag("walkAway"), Some(false));
        assert_eq!(flag("glide"), Some(false));
        let sound = glib::VariantDict::new(dict.lookup_value("sound", None).as_ref());
        assert_eq!(sound.lookup::<bool>("deskSound").unwrap(), Some(true));
        assert_eq!(sound.lookup::<bool>("phoneMic").unwrap(), Some(true));
        assert_eq!(sound.lookup::<bool>("phoneSound").unwrap(), Some(false));
        assert_eq!(dict.lookup::<i32>("rttMs").unwrap(), Some(12));
        assert_eq!(dict.lookup::<u32>("unread").unwrap(), Some(0));
        assert_eq!(
            dict.lookup_value("transfers", None)
                .unwrap()
                .type_()
                .as_str(),
            "aa{sv}"
        );
        let desk = Desk {
            keep_awake: false,
            clipboard: true,
            hand_off: false,
            pill: false,
            forward_notifications: true,
            pause_media_on_call: false,
            desk_mic: false,
            sound_available: true,
            dnd_supported: false,
            has_password: true,
        }
        .to_variant();
        let desk = glib::VariantDict::new(Some(&desk));
        assert_eq!(
            desk.lookup::<bool>("forwardNotifications").unwrap(),
            Some(true)
        );
        assert_eq!(desk.lookup::<bool>("hasPassword").unwrap(), Some(true));
    }

    #[test]
    fn a_transfer_in_flight_is_published_with_its_lane() {
        use crate::device_links::webrtc::transfer_board;
        let row = transfer_board::TransferRow {
            id: "shell-t1".into(),
            device_id: "shell-lane".into(),
            name: "film.mkv".into(),
            incoming: false,
            done: 250,
            total: 1000,
            preparing: false,
            paused: false,
        };
        assert!(transfer_board::begin(row));
        transfer_board::set_lane("shell-t1", Some("fast lane"));
        let shown = shell_transfers("shell-lane");
        transfer_board::discard("shell-t1");
        assert_eq!(shown.len(), 1);
        assert_eq!(
            (shown[0].permille, shown[0].lane.as_str(), shown[0].incoming),
            (250, "fast lane", false)
        );
        assert!(shell_transfers("shell-lane").is_empty());
    }

    #[test]
    fn every_page_action_names_a_page_the_window_has() {
        for (action, page) in PAGES {
            let (resolved, _) = crate::window::resolve_page(page);
            assert!(
                [
                    "home",
                    "files",
                    "notes",
                    "notifications",
                    "contacts",
                    "terminals",
                    "settings"
                ]
                .contains(&resolved),
                "{action} -> {page}"
            );
        }
    }

    /// Phone names, notifications, snippets and permissions are for the Shell:
    /// another process on the bus reads empty values of the same type, and
    /// nothing is broadcast.
    #[test]
    fn only_the_shell_reads_the_private_properties() {
        let mut device = capable();
        device.name = "Secret phone".into();
        let settings = AppSettings::default();
        let devices = [shell_device(&device, &settings)];
        let values = std::collections::HashMap::from([
            ("Version", VERSION.to_variant()),
            ("Desk", desk(&devices).to_variant()),
            ("Devices", devices_variant(&devices)),
            ("Notifications", notifications_variant(&[device], &settings)),
            ("Transfers", transfers_variant(&[])),
            ("Snippets", snippets_variant()),
            ("Activities", activities_variant(&[], &settings, 0)),
        ]);
        for (name, real) in &values {
            let seen = visible_property(name, &values, true);
            assert_eq!(&seen, real, "{name} for the Shell");
            let other = visible_property(name, &values, false);
            assert_eq!(other.type_(), real.type_(), "{name}: the same type");
            if *name == "Version" {
                assert_eq!(other, *real);
            } else {
                assert_eq!(other.n_children(), 0, "{name} is empty for anyone else");
                assert!(!other.print(true).contains("Secret"), "{name}");
            }
        }
        assert!(
            visible_property("Devices", &values, true)
                .print(true)
                .contains("Secret phone"),
            "the Shell sees the name"
        );
        // No broadcast: every PropertiesChanged names its destination.
        let source = include_str!("shell_dbus.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap();
        assert_eq!(source.matches(".emit_signal(").count(), 1);
        assert!(source.contains("emit_signal(\n        Some(destination),"));
    }
}
