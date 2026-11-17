//! App-wide preferences ([`AppSettings`]) and the in-memory mirrors other
//! threads read without the config lock: the settings, per-device terminal
//! access and remembered remote-desktop grants.
//!
//! Sibling edges: none (the parent's `Config` for the mirrors' sources).

use super::*;

/// App-wide preferences, changed from the Preferences window.
///
/// Every field defaults so a config written before it existed still loads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppSettings {
    /// Settings a newer build wrote that this one does not know.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
    /// Keep the computer awake, lid closed included, while a phone is paired,
    /// so it stays reachable from that phone.
    pub stay_awake: bool,
    /// Asked of a phone before it may use the screen or terminals. `None`
    /// means pairing alone is enough.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_password: Option<crate::device_links::session_lock::PasswordHash>,
    /// Ignored: kept so config.json round-trips. `relay_link::RELAY_ENABLED`
    /// alone decides whether the relay is used.
    pub relay_enabled: bool,
    /// A self-hosted relay (`wss://host/v1`); empty means the built-in one.
    /// Set in config.json only (no UI).
    pub relay_url: String,
    /// Ignored, like `relay_enabled`: TURN is used exactly when the relay is.
    pub use_turn: bool,
    /// Start in the background at login. `None` until decided: the first run
    /// turns it on, as a connection app is useless after a reboot otherwise.
    pub start_at_login: Option<bool>,
    /// The user declined the offer to show DeskLink in the GNOME top bar.
    pub shell_extension_prompt_dismissed: bool,
    /// The first-run welcome tour has been shown (finished, skipped or left
    /// for a pairing). A new key: nothing older is read or rewritten.
    pub onboarding_seen: bool,
    /// Share the clipboard with paired phones, both ways.
    pub clipboard_sync: bool,
    /// Phones whose notifications are not shown here; every phone's are by
    /// default.
    #[serde(skip_serializing_if = "std::collections::BTreeSet::is_empty")]
    pub hidden_notifications: std::collections::BTreeSet<String>,
    /// Phones not told when a long command in a shared terminal finishes;
    /// every phone is by default.
    #[serde(skip_serializing_if = "std::collections::BTreeSet::is_empty")]
    pub muted_command_alerts: std::collections::BTreeSet<String>,
    /// The phones that may switch "keep this computer awake" on or off from
    /// their side. None until turned on here, per phone: holding a laptop
    /// awake with the lid closed is the person at this desk's decision.
    #[serde(skip_serializing_if = "std::collections::BTreeSet::is_empty")]
    pub keep_awake_phones: std::collections::BTreeSet<String>,
    /// The phones that may unlock this computer from this network, approved
    /// on the phone each time. None until turned on here, per phone; never
    /// from the phone.
    #[serde(skip_serializing_if = "std::collections::BTreeSet::is_empty")]
    pub unlock_devices: std::collections::BTreeSet<String>,
    /// The phones whose walking away locks this computer (a minute out of
    /// reach, after a warning). None until turned on here, per phone.
    #[serde(skip_serializing_if = "std::collections::BTreeSet::is_empty")]
    pub walk_away_devices: std::collections::BTreeSet<String>,
    /// The phones whose new screenshots and photos are accepted (W12). None
    /// until turned on here, per phone.
    #[serde(skip_serializing_if = "std::collections::BTreeSet::is_empty")]
    pub gallery_devices: std::collections::BTreeSet<String>,
    /// The phones Do Not Disturb is shared with (W14). None until turned on
    /// here, per phone.
    #[serde(skip_serializing_if = "std::collections::BTreeSet::is_empty")]
    pub dnd_devices: std::collections::BTreeSet<String>,
    /// Show the phone as its own item in the top bar too (the extension's
    /// pill), besides the Quick Settings menu. Off by default.
    #[serde(default)]
    pub top_bar_pill: bool,
    /// The last folders a phone started a terminal in, newest first.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub recent_folders: Vec<String>,
    /// Preferences → Notifications to Phone: send this computer's
    /// notifications to the phones (off by default), only from these apps
    /// (app id, desktop entry or name).
    pub forward_notifications: bool,
    #[serde(skip_serializing_if = "std::collections::BTreeSet::is_empty")]
    pub forward_apps: std::collections::BTreeSet<String>,
    /// Preferences → Sound: a phone may ask to hear this computer's
    /// microphone (F1 A3; off by default).
    pub desk_mic_to_phone: bool,
    /// Preferences → Calls: the phones whose callers are named from the
    /// contacts they share (F5 K4; off by default, per phone).
    #[serde(skip_serializing_if = "std::collections::BTreeSet::is_empty")]
    pub contact_caller_names: std::collections::BTreeSet<String>,
    /// Glide (F4): the phone this computer's pointer glides onto, and the
    /// edge of this screen it sits at. `None` (the default) is off; one
    /// phone at a time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub glide: Option<crate::device_links::glide::GlideSetting>,
    /// Pause this computer's music while the phone is in a call, and resume
    /// it after (on by default).
    pub pause_media_on_call: bool,
    /// Show a phone's calls here (bar, notification, ring). On by default.
    pub calls_on_computer: bool,
    /// Keep the caller's name and number on a notification while this
    /// computer is locked. Off by default: a lock screen is public.
    pub caller_on_lock_screen: bool,
    /// Play a soft sound here while a phone rings (on by default; quiet in
    /// Do Not Disturb or with "Show calls" off).
    pub ring_on_computer: bool,
    /// Ask the update server for the newest version once a day (on by
    /// default; the update module reads it, nothing else does).
    pub update_check: bool,
    /// "beta", "stable" or "" (the default for the line: beta below 1.0).
    pub update_channel: String,
    /// Write "Send to phone" into the file managers' menus (on by default;
    /// off removes every file written for it).
    pub file_manager_send: bool,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            extra: serde_json::Map::new(),
            stay_awake: false,
            session_password: None,
            relay_enabled: true,
            relay_url: String::new(),
            use_turn: true,
            start_at_login: None,
            shell_extension_prompt_dismissed: false,
            onboarding_seen: false,
            clipboard_sync: true,
            hidden_notifications: Default::default(),
            muted_command_alerts: Default::default(),
            keep_awake_phones: Default::default(),
            unlock_devices: Default::default(),
            walk_away_devices: Default::default(),
            gallery_devices: Default::default(),
            dnd_devices: Default::default(),
            top_bar_pill: false,
            recent_folders: Vec::new(),
            forward_notifications: false,
            forward_apps: Default::default(),
            desk_mic_to_phone: false,
            contact_caller_names: Default::default(),
            glide: None,
            pause_media_on_call: true,
            calls_on_computer: true,
            caller_on_lock_screen: false,
            ring_on_computer: true,
            update_check: true,
            update_channel: String::new(),
            file_manager_send: true,
        }
    }
}

/// The relay used when the user has not set one. Release builds set
/// `DESKLINK_RELAY_URL` at build time to the deployed server
/// (relay/deploy/DEPLOY.md). Without it there is none: no relay is set up
/// until the user enters one, and nothing tries to reach one.
pub const DEFAULT_RELAY_URL: &str = match option_env!("DESKLINK_RELAY_URL") {
    Some(url) => url,
    None => "",
};

/// The relay to use: the user's, else `default`; empty when there is none.
pub(super) fn relay_url_or<'a>(chosen: &'a str, default: &'a str) -> &'a str {
    match chosen.trim() {
        "" => default.trim(),
        chosen => chosen,
    }
}

impl AppSettings {
    /// Whether `device_id`'s notifications are shown on this computer.
    pub fn shows_notifications(&self, device_id: &str) -> bool {
        !self.hidden_notifications.contains(device_id)
    }

    /// Whether `device_id` hears about long commands finishing.
    pub fn lets_keep_awake(&self, device_id: &str) -> bool {
        self.keep_awake_phones.contains(device_id)
    }

    pub fn alerts_commands_to(&self, device_id: &str) -> bool {
        !self.muted_command_alerts.contains(device_id)
    }

    /// Whether `device_id`'s callers are named from its shared contacts.
    pub fn names_callers_from(&self, device_id: &str) -> bool {
        self.contact_caller_names.contains(device_id)
    }

    /// Whether `device_id` may unlock this computer (Preferences, per phone).
    pub fn unlocks_from(&self, device_id: &str) -> bool {
        self.unlock_devices.contains(device_id)
    }

    /// Whether this computer locks when `device_id` walks away (per phone).
    pub fn locks_when_away(&self, device_id: &str) -> bool {
        self.walk_away_devices.contains(device_id)
    }

    /// Whether screenshots and photos from `device_id` are accepted (W12).
    pub fn accepts_gallery_from(&self, device_id: &str) -> bool {
        self.gallery_devices.contains(device_id) || gallery_forced_for_tests(device_id)
    }

    /// Whether Do Not Disturb is shared with `device_id` (W14).
    pub fn shares_dnd_with(&self, device_id: &str) -> bool {
        self.dnd_devices.contains(device_id)
    }

    /// The relay's address; empty when none is set up.
    pub fn relay_url(&self) -> &str {
        relay_url_or(&self.relay_url, DEFAULT_RELAY_URL)
    }

    /// Whether a relay server is set up at all. Never while the relay is
    /// compiled out (`relay_link::RELAY_ENABLED`): then nothing relay-related
    /// runs, is offered to the phone, or can be switched on from it.
    pub fn relay_configured(&self) -> bool {
        crate::device_links::relay_link::RELAY_ENABLED && !self.relay_url().is_empty()
    }

    /// Whether DeskLink should reach paired phones through the relay: compiled
    /// in, and a relay to reach. The stored `relay_enabled` is not read.
    pub fn relay_wanted(&self) -> bool {
        self.relay_configured()
    }
}

/// The current [`AppSettings`], readable without a [`Config`]: the UI and the
/// peer workers need them, the daemon worker owns the `Config`. Same mirror
/// rule as terminal consent: only `Config` writes it, in the call that
/// persists the change.
pub(super) fn settings_mirror() -> &'static Mutex<AppSettings> {
    static MIRROR: OnceLock<Mutex<AppSettings>> = OnceLock::new();
    MIRROR.get_or_init(Default::default)
}

pub fn app_settings() -> AppSettings {
    settings_mirror()
        .lock()
        .map(|settings| settings.clone())
        // A poisoned mirror still holds the last settings. Defaults here would
        // read as "no session password" and open the gate it guards.
        .unwrap_or_else(|poisoned| poisoned.into_inner().clone())
}

/// Tests: devices a test turned photos on for. Kept apart from the settings
/// mirror: any other test loading or saving settings replaces the mirror (the
/// flaky `a_gallery_image_is_refused_unless_turned_on...`).
#[cfg(test)]
static GALLERY_FOR_TESTS: Mutex<Vec<String>> = Mutex::new(Vec::new());

#[cfg(test)]
fn gallery_forced_for_tests(device_id: &str) -> bool {
    GALLERY_FOR_TESTS
        .lock()
        .is_ok_and(|on| on.iter().any(|id| id == device_id))
}

#[cfg(not(test))]
fn gallery_forced_for_tests(_device_id: &str) -> bool {
    false
}

/// Tests: turns photos from `device_id` on or off, whatever the settings say.
#[cfg(test)]
pub(crate) fn set_gallery_for_tests(device_id: &str, on: bool) {
    if let Ok(mut forced) = GALLERY_FOR_TESTS.lock() {
        forced.retain(|id| id != device_id);
        if on {
            forced.push(device_id.to_string());
        }
    }
}

pub(super) fn mirror_settings(settings: &AppSettings) {
    if let Ok(mut mirror) = settings_mirror().lock() {
        *mirror = settings.clone();
    }
}

/// Devices currently allowed a terminal, readable without a [`Config`].
///
/// The terminal bridge enforces consent on the peer worker thread and the UI's
/// once-a-second poll shows it; neither owns the `Config` the daemon worker
/// holds. This is a mirror, not a second source of truth: every `Config`
/// method that changes a device's consent — trusting, untrusting, the setting
/// itself, loading from disk — updates it in the same call, so it cannot drift
/// from what was persisted. Anything unknown, or a poisoned lock, reads as
/// *not allowed*.
fn terminal_access_mirror() -> &'static Mutex<HashSet<String>> {
    static MIRROR: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    MIRROR.get_or_init(|| Mutex::new(HashSet::new()))
}

pub(super) fn mirror_terminal_access(device_id: &str, allowed: bool) {
    if let Ok(mut mirror) = terminal_access_mirror().lock() {
        if allowed {
            mirror.insert(device_id.to_string());
        } else {
            mirror.remove(device_id);
        }
    }
}

/// May `device_id` open terminals on this computer right now?
pub fn terminal_access_allowed(device_id: &str) -> bool {
    terminal_access_mirror()
        .lock()
        .is_ok_and(|mirror| mirror.contains(device_id))
}

/// GNOME remote-desktop restore tokens, one per paired device.
///
/// A token lets a later session resume a grant the user approved at this
/// desktop without the consent dialog — the difference between controlling
/// the computer from the phone alone and needing someone at the desk to click
/// Share. GNOME honours it only while this process runs (see
/// `PERSIST_FOR_SESSION`), so it lives in memory: on disk it would outlive the
/// grant and make the UI claim an approval that no longer exists. It belongs
/// to the pairing: unpairing or a new certificate removes it. Single-use: the
/// portal hands back a fresh one on every start.
fn remote_desktop_tokens() -> &'static Mutex<HashMap<String, String>> {
    static TOKENS: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    TOKENS.get_or_init(Default::default)
}

/// The remembered remote-control grant for `device_id`, if any.
pub(crate) fn remote_desktop_token(device_id: &str) -> Option<String> {
    remote_desktop_tokens().lock().ok()?.get(device_id).cloned()
}

/// Remembers (or with `None`, forgets) the grant for `device_id`.
///
/// While a phone holds a grant it can take control again at any time, so
/// accessibility stays on: apps opened meanwhile (Chrome) report their text
/// fields and the phone keyboard can follow them.
pub(crate) fn set_remote_desktop_token(device_id: &str, token: Option<&str>) {
    if let Ok(mut tokens) = remote_desktop_tokens().lock() {
        match token {
            Some(token) => tokens.insert(device_id.to_string(), token.to_string()),
            None => tokens.remove(device_id),
        };
    }
    // Off the caller's thread (the GTK one, from the device page): blocking
    // D-Bus calls. Not in tests, which must not change the desktop's setting.
    #[cfg(not(test))]
    {
        let (holder, wanted) = (format!("grant-{device_id}"), token.is_some());
        let _ = std::thread::Builder::new()
            .name("desklink-a11y".into())
            .spawn(move || {
                crate::device_links::webrtc::text_focus::hold_accessibility(&holder, wanted)
            });
    }
}
