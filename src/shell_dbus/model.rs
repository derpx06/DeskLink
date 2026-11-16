//! What the top bar is told about each phone and the desk: [`ShellDevice`],
//! its transfers, Bucket, media and call, the live stamps, and which actions
//! a phone is offered now (`actions`, `INSERTS`, `PAGES`, `DEVICELESS`), and
//! its switches (which settings it may show and flip, and the daemon command
//! a flip becomes). Pure data from the daemon's `DeviceView`s and the
//! settings; no D-Bus.
//!
//! Sibling edges: none. Outside: the daemon's `DeviceView`, `config`, `folders`,
//! `glide`, `desk_features`, `webrtc`, `bucket_progress`, `terminal_previews`,
//! `window` (`calls`, `can_send_feature`).

use super::*;

/// Actions that need no device (`device_id` ""): `end-sessions` then means
/// every device.
pub(super) const DEVICELESS: [&str; 10] = [
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
pub(super) struct Desk {
    pub(super) keep_awake: bool,
    pub(super) clipboard: bool,
    /// A terminal is running and a phone is connected.
    pub(super) hand_off: bool,
    /// Settings → Appearance → "Phone in the top bar": the extension's own pill, on
    /// top of the Quick Settings menu.
    pub(super) pill: bool,
    /// This computer's notifications go to the phones.
    pub(super) forward_notifications: bool,
    /// This computer's players pause during a call.
    pub(super) pause_media_on_call: bool,
    /// Phones may hear this computer's microphone.
    pub(super) desk_mic: bool,
    /// Sound can work here (PipeWire and its GStreamer plugin).
    pub(super) sound_available: bool,
    /// Do Not Disturb can be shared (GNOME's notification settings).
    pub(super) dnd_supported: bool,
    /// A session password is set.
    pub(super) has_password: bool,
}

impl Desk {
    pub(super) fn to_variant(&self) -> Variant {
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

pub(super) fn desk(devices: &[ShellDevice]) -> Desk {
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
pub(super) struct ShellDevice {
    pub(super) id: String,
    pub(super) name: String,
    pub(super) kind: String,
    pub(super) paired: bool,
    pub(super) reachable: bool,
    pub(super) ready: bool,
    pub(super) battery: i32,
    pub(super) charging: bool,
    pub(super) path: String,
    pub(super) screen: &'static str,
    pub(super) terminals: u32,
    /// When its link was lost (Unix seconds), while it stays lost; 0 live.
    pub(super) last_seen: u64,
    /// Its call now, live links only.
    pub(super) call: Option<ShellCall>,
    /// The phone's music now, live links only.
    pub(super) media: Option<ShellMedia>,
    /// Its Bucket sync, while one is going.
    pub(super) bucket: Option<ShellBucket>,
    /// Since when it has been using this computer (Unix seconds); 0 not.
    pub(super) live_since: u64,
    /// Its per-phone switches, by [`SWITCHES`] key.
    pub(super) settings: Vec<(&'static str, bool)>,
    /// Sound with this phone: this computer's to it, its microphone here, its
    /// sound here.
    pub(super) sound: [bool; 3],
    /// Round trip in milliseconds; -1 unknown.
    pub(super) rtt_ms: i32,
    /// Its notifications shown here and not yet dismissed.
    pub(super) unread: u32,
    /// The phone can read text from a picture.
    pub(super) text_capture: bool,
    /// Files going to or from it now (the Bucket's are in `bucket`).
    pub(super) transfers: Vec<ShellTransfer>,
    pub(super) actions: Vec<&'static str>,
}

/// One file moving now: `Devices[].transfers`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ShellTransfer {
    pub(super) name: String,
    pub(super) incoming: bool,
    /// 0..=1000; 0 while still preparing.
    pub(super) permille: u32,
    pub(super) paused: bool,
    /// "fast lane" or "over the secure link"; "" not said yet.
    pub(super) lane: String,
}

pub(super) fn shell_transfers(device_id: &str) -> Vec<ShellTransfer> {
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
pub(super) struct ShellBucket {
    /// "Syncing 3 files · 1.1 GB of 2.4 GB".
    pub(super) line: String,
    /// "12 MB/s · about 4 min left".
    pub(super) detail: String,
    /// 0..=1000.
    pub(super) permille: u32,
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
pub(super) struct ShellMedia {
    pub(super) title: String,
    pub(super) artist: String,
    pub(super) playing: bool,
    pub(super) position_ms: i64,
    pub(super) length_ms: i64,
    /// Why the timeline cannot be used; empty when it can.
    pub(super) seek_reason: String,
}

pub(super) fn shell_media(device: &DeviceView) -> Option<ShellMedia> {
    let media = device
        .media_status
        .as_ref()
        .filter(|media| device.feature_transport_ready && !media.title.is_empty())?;
    Some(ShellMedia {
        title: media.title.clone(),
        artist: media.artist.clone(),
        playing: media.is_playing,
        position_ms: media.position.unwrap_or(0),
        length_ms: media.length.unwrap_or(0),
        seek_reason: crate::device_links::media_seek::blocked(media.can_seek, media.length)
            .unwrap_or_default()
            .to_string(),
    })
}

thread_local! {
    /// When each device started using this computer, while it does.
    static LIVE_SINCE: RefCell<std::collections::HashMap<String, u64>> = RefCell::default();
}

/// Fills `live_since`: the first time each device was seen live, kept while
/// it stays live.
pub(super) fn stamp_live(devices: &mut [ShellDevice]) {
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
pub(super) struct ShellCall {
    /// The phone's id for it, the same for all its updates.
    pub(super) id: String,
    /// "ringing", "dialing", "talking" or "held".
    pub(super) state: &'static str,
    pub(super) incoming: bool,
    /// The contact, else the number, else "Incoming call" (the phone keeps
    /// the caller private unless its user allowed it).
    pub(super) caller: String,
    /// "" when not sent.
    pub(super) number: String,
    /// Unix milliseconds; 0 unknown.
    pub(super) started_at: i64,
    pub(super) muted: bool,
    pub(super) silenced: bool,
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

pub(super) fn shell_device(device: &DeviceView, settings: &AppSettings) -> ShellDevice {
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
        path: crate::device_links::device::shown_path(
            device.connection_path.as_deref().unwrap_or_default(),
        )
        .to_string(),
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
pub(super) const INSERTS: [(&str, usize); 5] = [
    ("insert-photo", 0),
    ("insert-photo-file", 1),
    ("insert-scan", 2),
    ("insert-text", 3),
    ("insert-qr", 4),
];

/// The pages of the window a client may open for a phone, by action.
pub(super) const PAGES: [(&str, &str); 8] = [
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
pub(super) fn actions(device: &DeviceView, settings: &AppSettings) -> Vec<&'static str> {
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
        if shell_media(device).is_some_and(|media| media.seek_reason.is_empty()) {
            actions.push("media-seek");
        }
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

// ── Switches: which settings the top bar may show and flip ──────────────

/// Whose switch it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Scope {
    Phone,
    Desk,
}

/// A switch a client may set with `SetSwitch`. `sensitive` ones are off by
/// default for a security reason (they let a phone run commands, unlock or
/// fill the disk, or let a phone hear this room); they can only be turned
/// OFF from the top bar. Turning one on is DeskLink's own page.
pub(super) struct SwitchSpec {
    pub(super) key: &'static str,
    pub(super) scope: Scope,
    pub(super) sensitive: bool,
}

const fn switch(key: &'static str, scope: Scope, sensitive: bool) -> SwitchSpec {
    SwitchSpec {
        key,
        scope,
        sensitive,
    }
}

pub(super) const SWITCHES: [SwitchSpec; 18] = [
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
pub(super) fn phone_switch(key: &str, device: &DeviceView, settings: &AppSettings) -> Option<bool> {
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
pub(super) fn resolve_switch(
    key: &str,
    device_id: &str,
    on: bool,
) -> Result<DaemonCommand, String> {
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
pub(super) fn switch_allowed(
    device: Option<&ShellDevice>,
    key: &str,
    on: bool,
) -> Result<(), String> {
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

/// How many recent files the menu lists.
pub(super) const RECENT_TRANSFERS: usize = 5;
