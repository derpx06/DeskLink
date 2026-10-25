use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use openssl::asn1::Asn1Time;
use openssl::bn::{BigNum, MsbOption};
use openssl::ec::{EcGroup, EcKey};
use openssl::hash::MessageDigest;
use openssl::nid::Nid;
use openssl::pkey::{PKey, Private};
use openssl::x509::{X509NameBuilder, X509};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::device_info::DeviceInfo;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrustedDevice {
    pub id: String,
    pub name: String,
    pub device_type: String,
    pub protocol_version: i64,
    pub certificate_pem: String,
    /// Whether this phone may open terminals on this computer. **Off unless the
    /// user turned it on**, and absent in configs written before the feature
    /// existed, which therefore load as off.
    #[serde(default)]
    pub terminal_access: bool,
}

/// The format of `config.json` this build writes. A build meeting a *newer*
/// file reads what it understands and does not rewrite it (a downgrade used to
/// erase every setting it did not know); every build keeps the fields it does
/// not know when it writes its own.
const CONFIG_VERSION: u32 = 1;

fn config_version() -> u32 {
    CONFIG_VERSION
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredConfig {
    #[serde(default = "config_version")]
    version: u32,
    /// Fields a newer build wrote and this one does not know, kept as they
    /// were.
    #[serde(flatten)]
    extra: serde_json::Map<String, serde_json::Value>,
    device_id: String,
    device_name: String,
    trusted_devices: HashMap<String, TrustedDevice>,
    /// ICE servers, empty by default.
    ///
    /// Operation on a local network must never need one, so nothing is shipped
    /// preconfigured — not even a public STUN server. These matter only once
    /// the two devices are on different networks.
    #[serde(default)]
    ice_stun_servers: Vec<String>,
    #[serde(default)]
    ice_turn_servers: Vec<String>,
    #[serde(default)]
    settings: AppSettings,
}

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
    /// Reach paired phones through the DeskLink relay when they are not on
    /// this network.
    pub relay_enabled: bool,
    /// A self-hosted relay (`wss://host/v1`); empty means the built-in one.
    pub relay_url: String,
    /// Allow media through a TURN server when no direct path exists. Off
    /// means direct paths only, user-configured TURN servers included.
    pub use_turn: bool,
    /// Start in the background at login. `None` until decided: the first run
    /// turns it on, as a connection app is useless after a reboot otherwise.
    pub start_at_login: Option<bool>,
    /// The user declined the offer to show DeskLink in the GNOME top bar.
    pub shell_extension_prompt_dismissed: bool,
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
    /// The phones that may unlock this computer from this network, approved
    /// on the phone each time. None until turned on here, per phone; never
    /// from the phone.
    #[serde(skip_serializing_if = "std::collections::BTreeSet::is_empty")]
    pub unlock_devices: std::collections::BTreeSet<String>,
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
            clipboard_sync: true,
            hidden_notifications: Default::default(),
            muted_command_alerts: Default::default(),
            unlock_devices: Default::default(),
            top_bar_pill: false,
            recent_folders: Vec::new(),
            forward_notifications: false,
            forward_apps: Default::default(),
            desk_mic_to_phone: false,
            contact_caller_names: Default::default(),
            glide: None,
            pause_media_on_call: true,
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
fn relay_url_or<'a>(chosen: &'a str, default: &'a str) -> &'a str {
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

    /// The relay's address; empty when none is set up.
    pub fn relay_url(&self) -> &str {
        relay_url_or(&self.relay_url, DEFAULT_RELAY_URL)
    }

    /// Whether a relay server is set up at all.
    pub fn relay_configured(&self) -> bool {
        !self.relay_url().is_empty()
    }

    /// Whether DeskLink should reach paired phones through the relay: on,
    /// and a relay to reach.
    pub fn relay_wanted(&self) -> bool {
        self.relay_enabled && self.relay_configured()
    }
}

/// The current [`AppSettings`], readable without a [`Config`]: the UI and the
/// peer workers need them, the daemon worker owns the `Config`. Same mirror
/// rule as terminal consent: only `Config` writes it, in the call that
/// persists the change.
fn settings_mirror() -> &'static Mutex<AppSettings> {
    static MIRROR: OnceLock<Mutex<AppSettings>> = OnceLock::new();
    MIRROR.get_or_init(Default::default)
}

pub fn app_settings() -> AppSettings {
    settings_mirror()
        .lock()
        .map(|settings| settings.clone())
        .unwrap_or_default()
}

fn mirror_settings(settings: &AppSettings) {
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

fn mirror_terminal_access(device_id: &str, allowed: bool) {
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
pub(crate) fn set_remote_desktop_token(device_id: &str, token: Option<&str>) {
    if let Ok(mut tokens) = remote_desktop_tokens().lock() {
        match token {
            Some(token) => tokens.insert(device_id.to_string(), token.to_string()),
            None => tokens.remove(device_id),
        };
    }
}

/// What features removed in 1.3 left in `base_dir`: the automations' sealed
/// rules and their key.
const REMOVED_FEATURE_DIRS: [&str; 2] = ["automations", "transfers"];

/// Files that older builds kept and nothing reads now: the screen-sharing
/// restore tokens (grants are held in memory only, and cleared on unpair).
const REMOVED_FEATURE_FILES: [&str; 1] = ["remote_desktop_tokens.json"];

/// Deletes what removed features stored in `base_dir`. Whether any was there.
fn remove_stored_removed_features(base_dir: &Path) -> bool {
    let dirs = REMOVED_FEATURE_DIRS
        .iter()
        .filter(|name| fs::remove_dir_all(base_dir.join(name)).is_ok())
        .count();
    let files = REMOVED_FEATURE_FILES
        .iter()
        .filter(|name| fs::remove_file(base_dir.join(name)).is_ok())
        .count();
    dirs + files > 0
}

/// Moves the resumable-transfer partials (up to a gigabyte each) out of the
/// config directory, which backup and dotfile tools copy, into the state
/// directory. Quiet and once: only when the new place does not exist yet.
fn migrate_transfer_state(old: &Path, new: &Path) {
    if !old.is_dir() || new.exists() {
        return;
    }
    if let Some(parent) = new.parent() {
        if super::private_fs::create_dir_all(parent).is_err() {
            return;
        }
    }
    if fs::rename(old, new).is_ok() {
        return;
    }
    // Another filesystem: copy what is there, then remove the old.
    if super::private_fs::create_dir_all(new).is_err() {
        return;
    }
    let mut all_copied = true;
    if let Ok(entries) = fs::read_dir(old) {
        for entry in entries.flatten() {
            if entry.path().is_file()
                && fs::copy(entry.path(), new.join(entry.file_name())).is_err()
            {
                all_copied = false;
            }
        }
    }
    if all_copied {
        let _ = fs::remove_dir_all(old);
    }
}

/// A pairing record that a newer pairing may be the reinstalled same phone
/// of: same name and type, another id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplacementCandidate {
    pub old_id: String,
    pub new_id: String,
    pub name: String,
}

fn replacement_candidates() -> &'static Mutex<Vec<ReplacementCandidate>> {
    static CANDIDATES: std::sync::OnceLock<Mutex<Vec<ReplacementCandidate>>> =
        std::sync::OnceLock::new();
    CANDIDATES.get_or_init(Default::default)
}

/// The older pairing records that a pairing which just succeeded might
/// replace, since last asked. Nothing is removed here: the daemon decides
/// (only a record with no live session, that looks like the same machine) and
/// then calls [`Config::untrust_device`].
pub fn take_replacement_candidates() -> Vec<ReplacementCandidate> {
    replacement_candidates()
        .lock()
        .map(|mut candidates| std::mem::take(&mut *candidates))
        .unwrap_or_default()
}

pub struct Config {
    base_dir: PathBuf,
    stored: StoredConfig,
    key: PKey<Private>,
    certificate: X509,
    /// The file was written by a newer build: read, never rewritten.
    read_only: bool,
    /// What start-up wants the person told (a damaged settings file set
    /// aside, a newer file kept read-only), taken once.
    load_notice: Option<String>,
}

fn now_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis())
        .unwrap_or_default()
}

/// Settings that features removed in 1.3 stored (dropped on load; every other
/// unknown key is kept for a build that knows it).
const REMOVED_SETTING_KEYS: [&str; 3] = ["run_commands", "call_audio", "call_audio_devices"];

impl Config {
    pub fn load() -> Result<Self, String> {
        Self::load_from_dir(config_dir())
    }

    pub fn load_from_dir(base_dir: PathBuf) -> Result<Self, String> {
        super::private_fs::create_dir_all(&base_dir).map_err(|err| err.to_string())?;
        let state_path = base_dir.join("config.json");
        let mut notice = None;
        let parsed = if state_path.exists() {
            let bytes = fs::read(&state_path).map_err(|error| {
                format!(
                    "DeskLink could not read its settings file {}: {error}",
                    state_path.display()
                )
            })?;
            match serde_json::from_slice::<StoredConfig>(&bytes) {
                Ok(stored) => Some(stored),
                Err(_) => {
                    // Damaged by something else (a restore, an editor, a full
                    // disk): kept beside it, never deleted, and DeskLink
                    // starts fresh, saying so in words.
                    let aside = base_dir.join(format!("config.corrupt-{}", now_millis()));
                    let kept = fs::rename(&state_path, &aside).is_ok();
                    notice = Some(format!(
                        "DeskLink's settings file was damaged, so it started fresh. Your phones need pairing again.{}",
                        if kept {
                            format!(" The damaged file is kept as {}.", aside.display())
                        } else {
                            String::new()
                        }
                    ));
                    None
                }
            }
        } else {
            None
        };
        let read_only = parsed
            .as_ref()
            .is_some_and(|stored| stored.version > CONFIG_VERSION);
        if read_only {
            notice = Some(
                "These settings were written by a newer DeskLink. This version reads them but does not change them."
                    .to_string(),
            );
        }
        let mut stored = if let Some(stored) = parsed {
            stored
        } else {
            let device_id = Uuid::new_v4().simple().to_string();
            let device_name = hostname::get()
                .ok()
                .and_then(|name| name.into_string().ok())
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| "DeskLink".to_string());
            StoredConfig {
                version: CONFIG_VERSION,
                extra: serde_json::Map::new(),
                device_id,
                device_name,
                trusted_devices: HashMap::new(),
                ice_stun_servers: Vec::new(),
                ice_turn_servers: Vec::new(),
                settings: AppSettings::default(),
            }
        };

        let key_path = base_dir.join("privateKey.pem");
        let cert_path = base_dir.join("certificate.pem");
        let key = if key_path.exists() {
            PKey::private_key_from_pem(&fs::read(&key_path).map_err(|err| err.to_string())?)
                .map_err(|err| err.to_string())?
        } else {
            generate_key().map_err(|err| err.to_string())?
        };
        let stored_certificate = if cert_path.exists() {
            fs::read(&cert_path)
                .ok()
                .and_then(|bytes| X509::from_pem(&bytes).ok())
        } else {
            None
        };
        // A truncated or malformed local certificate must not prevent DeskLink
        // from starting. The private key and device ID remain authoritative, so
        // rebuild only the certificate and keep all pairing/configuration state
        // untouched.
        //
        // The same is true when the certificate is well-formed but no longer
        // matches the private key — which happens whenever the key file is lost
        // or restored independently of the certificate. Keeping the mismatched
        // pair made every TLS handshake fail permanently, and `save()` then
        // wrote the broken pair straight back to disk.
        let certificate = match stored_certificate {
            Some(certificate) if certificate_matches_key(&certificate, &key) => certificate,
            _ => generate_certificate(&key, &stored.device_id).map_err(|err| err.to_string())?,
        };

        // What features that no longer exist left among the settings.
        for gone in REMOVED_SETTING_KEYS {
            stored.settings.extra.remove(gone);
        }
        for device in stored.trusted_devices.values() {
            mirror_terminal_access(&device.id, device.terminal_access);
        }
        mirror_settings(&stored.settings);
        // Features that were removed: what they stored goes, quietly. The
        // settings that went with them are dropped by the save below
        // (unknown keys are not kept); pairing records and the rest stay.
        remove_stored_removed_features(&base_dir);
        let config = Self {
            base_dir,
            stored,
            key,
            certificate,
            read_only,
            load_notice: notice,
        };
        if let Some(notice) = &config.load_notice {
            eprintln!("[Config] {notice}");
        }
        // Written only when something differs, and a failure to write is no
        // reason not to run: a full or read-only disk keeps the config in
        // memory, and the next change tries again.
        if let Err(error) = config.save() {
            crate::device_links::limits::log_limited(
                "config-save",
                &format!("Could not save the settings; carrying on with them in memory: {error}"),
            );
        }
        Ok(config)
    }

    /// Every write is atomic (temp file, fsync, rename, directory fsync), and
    /// the key and certificate are only written when they differ from disk.
    /// `save` runs on every pair and unpair; truncating in place meant a crash
    /// mid-write left a half-written config.json or private key, `load` then
    /// failed, and DeskLink never started again without losing every pairing.
    pub fn save(&self) -> Result<(), String> {
        if self.read_only {
            return Ok(());
        }
        super::private_fs::create_dir_all(&self.base_dir).map_err(|err| err.to_string())?;
        // The pairing record and the session-password hash: this user only.
        write_atomic_if_changed(
            &self.base_dir.join("config.json"),
            &serde_json::to_vec_pretty(&self.stored).map_err(|err| err.to_string())?,
            0o600,
        )?;
        write_atomic_if_changed(
            &self.base_dir.join("privateKey.pem"),
            &self
                .key
                .private_key_to_pem_pkcs8()
                .map_err(|err| err.to_string())?,
            0o600,
        )?;
        write_atomic_if_changed(
            &self.base_dir.join("certificate.pem"),
            &self.certificate.to_pem().map_err(|err| err.to_string())?,
            0o600,
        )?;
        Ok(())
    }

    #[cfg(test)]
    fn set_device_name_for_test(&mut self, name: &str) {
        self.stored.device_name = name.to_string();
    }

    /// The start-up notice for the person, once.
    pub fn take_load_notice(&mut self) -> Option<String> {
        self.load_notice.take()
    }

    /// This computer's id: all a caller wants when it only compares ids.
    pub fn local_device_id(&self) -> &str {
        &self.stored.device_id
    }

    pub fn local_device_info(&self) -> DeviceInfo {
        DeviceInfo::local(
            self.stored.device_id.clone(),
            self.stored.device_name.clone(),
        )
    }

    pub fn key(&self) -> &PKey<Private> {
        &self.key
    }

    pub fn certificate(&self) -> &X509 {
        &self.certificate
    }

    /// Private state for resumable WebRTC file transfers.  This remains below
    /// the existing protocol directory so rebranding and transport migration
    /// never move identities or pairing records.
    pub fn transfer_state_dir(&self) -> PathBuf {
        let old = self.base_dir.join("webrtc-transfers");
        if self.base_dir != config_dir() {
            return old; // a test or another install root: where it always was
        }
        let new = dirs::state_dir()
            .or_else(dirs::data_local_dir)
            .map(|state| state.join("desklink").join("transfers"));
        match new {
            Some(new) => {
                migrate_transfer_state(&old, &new);
                new
            }
            None => old,
        }
    }

    /// Snippets and notes (F6), encrypted; never removed on unpair.
    pub fn notes_dir(&self) -> PathBuf {
        self.base_dir.join("notes")
    }

    /// Folder sync pairs and manifests (F8), encrypted.
    pub fn folders_dir(&self) -> PathBuf {
        self.base_dir.join("folders")
    }

    /// The phones' contact caches (F5), encrypted.
    pub fn contacts_dir(&self) -> PathBuf {
        self.base_dir.join("contacts")
    }

    /// ICE servers for the WebRTC peer.
    ///
    /// Environment variables win over the stored lists so a deployment can set
    /// them without editing config.json, using the same comma/newline grammar
    /// as the Android preferences. A malformed list is reported and ignored
    /// rather than failing the connection: local operation needs no server, so
    /// a bad remote-access setting must not break the LAN path.
    pub fn ice_servers(&self) -> crate::device_links::webrtc::ice::IceServerConfig {
        // Debug builds only: in a release, an environment variable must not be
        // able to route media through a server the user never chose.
        let env = |name: &str| {
            cfg!(debug_assertions)
                .then(|| std::env::var(name).ok())
                .flatten()
        };
        let stun =
            env("DESKLINK_STUN_SERVERS").unwrap_or_else(|| self.stored.ice_stun_servers.join(","));
        let turn =
            env("DESKLINK_TURN_SERVERS").unwrap_or_else(|| self.stored.ice_turn_servers.join(","));
        match crate::device_links::webrtc::ice::IceServerConfig::parse(&stun, &turn) {
            Ok(config) => config,
            Err(error) => {
                eprintln!("[DeskLink] Ignoring the configured ICE servers: {error}");
                crate::device_links::webrtc::ice::IceServerConfig::default()
            }
        }
    }

    /// Applies `change` to the settings and persists them. On a failed save
    /// nothing changes, on disk or in memory.
    pub fn update_settings(&mut self, change: impl FnOnce(&mut AppSettings)) -> Result<(), String> {
        let previous = self.stored.settings.clone();
        change(&mut self.stored.settings);
        if let Err(error) = self.save() {
            self.stored.settings = previous;
            return Err(error);
        }
        mirror_settings(&self.stored.settings);
        Ok(())
    }

    pub fn trusted_devices(&self) -> impl Iterator<Item = &TrustedDevice> {
        self.stored.trusted_devices.values()
    }

    pub fn has_trusted_devices(&self) -> bool {
        !self.stored.trusted_devices.is_empty()
    }

    pub fn is_trusted(&self, device_id: &str) -> bool {
        self.stored.trusted_devices.contains_key(device_id)
    }

    pub fn trusted_certificate_pem(&self, device_id: &str) -> Option<String> {
        self.stored
            .trusted_devices
            .get(device_id)
            .map(|device| device.certificate_pem.clone())
    }

    pub fn trusted_protocol_version(&self, device_id: &str) -> Option<i64> {
        self.stored
            .trusted_devices
            .get(device_id)
            .map(|device| device.protocol_version)
    }

    /// Lets `device_id` open terminals on this computer, or stops it.
    ///
    /// Fails for a device that is not paired: consent is a property of a trust
    /// record. It fails *closed* — if the setting cannot be written, a grant is
    /// not applied, and a revocation still is: the person who turned it off
    /// must not find it still on because the disk was full.
    pub fn set_terminal_access(&mut self, device_id: &str, allowed: bool) -> Result<(), String> {
        self.stored
            .trusted_devices
            .get_mut(device_id)
            .ok_or_else(|| "That device is not paired".to_string())?
            .terminal_access = allowed;
        let saved = self.save();
        let effective = allowed && saved.is_ok();
        if let Some(device) = self.stored.trusted_devices.get_mut(device_id) {
            device.terminal_access = effective;
        }
        mirror_terminal_access(device_id, effective);
        saved
    }

    pub fn trust_device(
        &mut self,
        info: &DeviceInfo,
        certificate_pem: String,
    ) -> Result<(), String> {
        // Re-confirming an existing pairing is not a new grant, and must not
        // quietly revoke one either: the same certificate is the same trust, so
        // the choice carries over. A different certificate is a different
        // device identity behind the same id, and consent starts again from off.
        let terminal_access = self
            .stored
            .trusted_devices
            .get(&info.id)
            .is_some_and(|existing| {
                existing.certificate_pem == certificate_pem && existing.terminal_access
            });
        let previous = self.stored.trusted_devices.get(&info.id).cloned();
        let same_identity = previous
            .as_ref()
            .is_some_and(|existing| existing.certificate_pem == certificate_pem);
        if !same_identity {
            // A new identity does not inherit remote control of this desktop,
            // nor the contacts the old one shared.
            set_remote_desktop_token(&info.id, None);
            crate::device_links::contacts::forget_in(&self.contacts_dir(), &info.id);
            crate::device_links::session_lock::forget_unlock(&info.id);
            // What the old identity said it already held is not what a new
            // one has: its snapshots are sent again.
            crate::device_links::sync::forget_device(&info.id);
            // "Unlock with this phone" is a consent for that identity.
            if self.stored.settings.unlock_devices.remove(&info.id) {
                mirror_settings(&self.stored.settings);
            }
        }
        mirror_terminal_access(&info.id, terminal_access);
        self.stored.trusted_devices.insert(
            info.id.clone(),
            TrustedDevice {
                id: info.id.clone(),
                name: info.name.clone(),
                device_type: info.device_type.clone(),
                protocol_version: info.protocol_version,
                certificate_pem,
                terminal_access,
            },
        );
        let is_new_record = previous.is_none();
        self.save_or_restore(&info.id, previous)?;
        if !same_identity {
            // The relay's allow list is built from the trust store.
            crate::device_links::relay_link::client::reset();
        }
        if is_new_record {
            self.queue_lookalikes(info);
        }
        Ok(())
    }

    /// A phone paired again under a new id — it was reinstalled, so its id
    /// and certificate are new — may be the same phone as an older record of
    /// the same name and type. That is only a guess (two phones of one model
    /// are both "realme 14 5G"), so the older records are queued for the
    /// daemon to judge (see [`take_replacement_candidates`]), not deleted.
    fn queue_lookalikes(&mut self, info: &DeviceInfo) {
        let candidates: Vec<ReplacementCandidate> = self
            .stored
            .trusted_devices
            .values()
            .filter(|known| {
                known.id != info.id
                    && known.name == info.name
                    && known.device_type == info.device_type
            })
            .map(|known| ReplacementCandidate {
                old_id: known.id.clone(),
                new_id: info.id.clone(),
                name: info.name.clone(),
            })
            .collect();
        if let Ok(mut queued) = replacement_candidates().lock() {
            queued.extend(candidates);
        }
    }

    /// Saves, or puts the device's previous record back if the save fails.
    /// Memory must agree with disk: the trust store is what decides pairing on
    /// every new link, so a record kept only in memory paired a device whose
    /// pairing would vanish on restart (and the reverse for an unpair).
    fn save_or_restore(
        &mut self,
        device_id: &str,
        previous: Option<TrustedDevice>,
    ) -> Result<(), String> {
        let saved = self.save();
        if saved.is_err() {
            match previous {
                Some(record) => {
                    mirror_terminal_access(device_id, record.terminal_access);
                    self.stored
                        .trusted_devices
                        .insert(device_id.to_string(), record);
                }
                None => {
                    mirror_terminal_access(device_id, false);
                    self.stored.trusted_devices.remove(device_id);
                }
            }
        }
        saved
    }

    pub fn untrust_device(&mut self, device_id: &str) -> Result<(), String> {
        let previous = self.stored.trusted_devices.remove(device_id);
        mirror_terminal_access(device_id, false);
        // Remote control of this desktop is a property of the pairing too,
        // and so is the contacts cache.
        set_remote_desktop_token(device_id, None);
        crate::device_links::contacts::forget_in(&self.contacts_dir(), device_id);
        crate::device_links::session_lock::forget_unlock(device_id);
        // The revs of what the phone sent: a re-paired phone resends it all
        // (a stale rev made the phone skip a snapshot the desk no longer had).
        crate::device_links::sync::forget_device(device_id);
        // Unlocking this computer from it, too: a later phone under the same
        // id starts from off.
        if self.stored.settings.unlock_devices.remove(device_id) {
            mirror_settings(&self.stored.settings);
        }
        // Glide onto it, too (F4).
        if self
            .stored
            .settings
            .glide
            .as_ref()
            .is_some_and(|glide| glide.device == device_id)
        {
            self.stored.settings.glide = None;
            mirror_settings(&self.stored.settings);
        }
        self.save_or_restore(device_id, previous)?;
        crate::device_links::relay_link::client::reset();
        Ok(())
    }
}

/// Confirms the stored certificate actually certifies the stored private key.
/// Comparing the public halves in DER is exact and avoids a signature check.
fn certificate_matches_key(certificate: &X509, key: &PKey<Private>) -> bool {
    match (
        certificate
            .public_key()
            .and_then(|public| public.public_key_to_der()),
        key.public_key_to_der(),
    ) {
        (Ok(certificate_public), Ok(key_public)) => certificate_public == key_public,
        _ => false,
    }
}

/// Writes the device private key so only the owning user can read it.
///
/// `fs::write` alone creates the file at 0644 under a typical umask, which left
/// the key that authenticates every pairing and signs every handover transcript
/// world-readable. The mode is applied before the bytes are written so the key
/// is never briefly readable on first creation.
fn write_atomic_if_changed(path: &std::path::Path, bytes: &[u8], mode: u32) -> Result<(), String> {
    if fs::read(path).is_ok_and(|current| current == bytes) {
        return Ok(());
    }
    write_atomic(path, bytes, mode)
}

/// Writes `bytes` to `path` atomically, readable by this user only.
pub(crate) fn write_private(path: &std::path::Path, bytes: &[u8]) -> Result<(), String> {
    write_atomic(path, bytes, 0o600)
}

/// Replaces `path` so a reader — or a crash — only ever sees the old file or
/// the complete new one. The mode is set at creation, so a private key is
/// never readable by others even for a moment.
fn write_atomic(path: &std::path::Path, bytes: &[u8], mode: u32) -> Result<(), String> {
    use std::io::Write;

    let tmp = path.with_extension("tmp");
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(mode);
    }
    #[cfg(not(unix))]
    let _ = mode;
    let mut file = options.open(&tmp).map_err(|err| err.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // A leftover temp file keeps its old mode when reopened; tighten it.
        let _ = file.set_permissions(fs::Permissions::from_mode(mode));
    }
    file.write_all(bytes).map_err(|err| err.to_string())?;
    file.sync_all().map_err(|err| err.to_string())?;
    drop(file);
    fs::rename(&tmp, path).map_err(|err| err.to_string())?;
    if let Some(dir) = path.parent() {
        if let Ok(dir) = fs::File::open(dir) {
            let _ = dir.sync_all();
        }
    }
    Ok(())
}

/// `~/.config/desklink`: the frozen storage root (the config lives in its
/// `protocol` folder).
pub(crate) fn desklink_config_root() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("desklink")
}

fn config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("desklink")
        .join("protocol")
}

fn generate_key() -> Result<PKey<Private>, openssl::error::ErrorStack> {
    let group = EcGroup::from_curve_name(Nid::X9_62_PRIME256V1)?;
    let key = EcKey::generate(&group)?;
    PKey::from_ec_key(key)
}

fn generate_certificate(
    key: &PKey<Private>,
    device_id: &str,
) -> Result<X509, openssl::error::ErrorStack> {
    let mut serial = BigNum::new()?;
    serial.rand(128, MsbOption::MAYBE_ZERO, false)?;
    let serial = serial.to_asn1_integer()?;

    let mut name = X509NameBuilder::new()?;
    name.append_entry_by_nid(Nid::COMMONNAME, device_id)?;
    let name = name.build();

    let mut builder = X509::builder()?;
    builder.set_version(2)?;
    builder.set_serial_number(&serial)?;
    builder.set_subject_name(&name)?;
    builder.set_issuer_name(&name)?;
    builder.set_pubkey(key)?;
    builder.set_not_before(Asn1Time::days_from_now(0)?.as_ref())?;
    builder.set_not_after(Asn1Time::days_from_now(3650)?.as_ref())?;
    builder.sign(key, MessageDigest::sha256())?;
    Ok(builder.build())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Settings written before clipboard sync and notification mirroring had
    /// switches load with both on, and the defaults stay out of the file.
    #[test]
    fn clipboard_and_notifications_default_on_for_older_settings() {
        let old: AppSettings = serde_json::from_str(r#"{"stay_awake":true}"#).unwrap();
        assert!(old.clipboard_sync);
        assert!(old.shows_notifications("phone"));
        let written = serde_json::to_string(&AppSettings::default()).unwrap();
        assert!(!written.contains("hidden_notifications"), "{written}");

        assert!(old.alerts_commands_to("phone"), "command alerts default on");
        assert!(
            !old.unlocks_from("phone"),
            "unlocking from a phone defaults off"
        );
        assert!(!written.contains("unlock_devices") && !written.contains("muted_command_alerts"));
        let mut one = AppSettings::default();
        one.unlock_devices.insert("phone".into());
        one.muted_command_alerts.insert("tablet".into());
        assert!(
            one.unlocks_from("phone") && !one.unlocks_from("tablet"),
            "per phone"
        );
        assert!(one.alerts_commands_to("phone") && !one.alerts_commands_to("tablet"));
        assert!(old.recent_folders.is_empty());
        assert!(!written.contains("recent_folders"));

        let mut hidden = AppSettings::default();
        hidden.hidden_notifications.insert("phone".into());
        assert!(!hidden.shows_notifications("phone"));
        assert!(hidden.shows_notifications("tablet"));
    }

    /// With no relay built in and none entered, nothing tries to reach one.
    #[test]
    fn no_relay_is_wanted_until_one_is_set_up() {
        assert_eq!(relay_url_or("", ""), "");
        assert_eq!(relay_url_or("  ", "wss://built.in/v1"), "wss://built.in/v1");
        assert_eq!(
            relay_url_or(" wss://mine.example/v1 ", ""),
            "wss://mine.example/v1"
        );
        let mut settings = AppSettings::default();
        if DEFAULT_RELAY_URL.is_empty() {
            assert!(settings.relay_enabled, "on by default once one exists");
            assert!(!settings.relay_configured());
            assert!(!settings.relay_wanted(), "empty default: client not wanted");
        }
        settings.relay_url = "wss://mine.example/v1".into();
        assert!(settings.relay_wanted());
        settings.relay_enabled = false;
        assert!(!settings.relay_wanted());
    }

    #[test]
    fn local_identity_is_stable_after_reload() {
        let dir = std::env::temp_dir().join(format!("desklink-test-{}", Uuid::new_v4()));
        let first = Config::load_from_dir(dir.clone()).unwrap();
        let first_id = first.local_device_info().id;
        drop(first);

        let second = Config::load_from_dir(dir.clone()).unwrap();
        assert_eq!(second.local_device_info().id, first_id);

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_certificate_that_no_longer_matches_the_key_is_rebuilt() {
        let dir = std::env::temp_dir().join(format!("desklink-mismatch-{}", Uuid::new_v4()));
        let first = Config::load_from_dir(dir.clone()).unwrap();
        let first_id = first.local_device_info().id;
        let stale_certificate = first.certificate().to_pem().unwrap();
        drop(first);

        // Losing the key while the certificate survives is the realistic case:
        // a partial restore, or a key wiped by a permissions repair.
        fs::remove_file(dir.join("privateKey.pem")).unwrap();

        let repaired = Config::load_from_dir(dir.clone()).unwrap();
        assert_eq!(repaired.local_device_info().id, first_id);
        assert_ne!(repaired.certificate().to_pem().unwrap(), stale_certificate);
        assert!(certificate_matches_key(
            repaired.certificate(),
            repaired.key()
        ));

        let _ = fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn the_private_key_is_not_world_readable() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("desklink-perm-{}", Uuid::new_v4()));
        let config = Config::load_from_dir(dir.clone()).unwrap();
        drop(config);

        let mode = fs::metadata(dir.join("privateKey.pem"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0, "group and other must have no access");

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn malformed_certificate_is_rebuilt_without_changing_identity() {
        let dir = std::env::temp_dir().join(format!("desklink-cert-test-{}", Uuid::new_v4()));
        let first = Config::load_from_dir(dir.clone()).unwrap();
        let first_id = first.local_device_info().id;
        drop(first);

        fs::write(dir.join("certificate.pem"), b"").unwrap();

        let repaired = Config::load_from_dir(dir.clone()).unwrap();
        assert_eq!(repaired.local_device_info().id, first_id);
        assert!(repaired
            .certificate()
            .to_pem()
            .unwrap()
            .starts_with(b"-----BEGIN CERTIFICATE-----"));

        let _ = fs::remove_dir_all(dir);
    }

    fn trusted_config(dir: &std::path::Path, id: &str, certificate: &str) -> Config {
        let mut config = Config::load_from_dir(dir.to_path_buf()).unwrap();
        config
            .trust_device(
                &DeviceInfo::local(id.to_string(), "Test phone".to_string()),
                certificate.to_string(),
            )
            .unwrap();
        config
    }

    /// Terminal access is a shell on this computer. It must be something the
    /// user turned on, never something a pairing or an old config implies.
    #[test]
    fn terminal_access_is_off_by_default_and_for_configs_that_predate_it() {
        let dir = std::env::temp_dir().join(format!("desklink-term-default-{}", Uuid::new_v4()));
        let id = format!("phone-{}", Uuid::new_v4().simple());
        let config = trusted_config(&dir, &id, "cert-a");
        assert!(!terminal_access_allowed(&id));
        drop(config);

        // A config.json written before the field existed has no such key.
        let path = dir.join("config.json");
        let mut json: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        json["trusted_devices"][&id]
            .as_object_mut()
            .unwrap()
            .remove("terminal_access");
        fs::write(&path, serde_json::to_vec(&json).unwrap()).unwrap();

        let reloaded = Config::load_from_dir(dir.clone()).unwrap();
        assert!(!reloaded.stored.trusted_devices[&id].terminal_access);
        assert!(!terminal_access_allowed(&id));
        let _ = fs::remove_dir_all(dir);
    }

    /// 1.3: what removed features stored is deleted on the first start, quietly;
    /// pairing records, the certificate and every other preference stay.
    #[test]
    fn the_stored_data_of_removed_features_goes_on_first_start_and_nothing_else() {
        let dir = std::env::temp_dir().join(format!("desklink-removed-{}", Uuid::new_v4()));
        let id = format!("phone-{}", Uuid::new_v4().simple());
        let config = trusted_config(&dir, &id, "cert-a");
        let device_id = config.stored.device_id.clone();
        drop(config);
        // As a 1.2 install left things: sealed automation rules and their
        // key, and run-command and call-audio settings in config.json.
        let automations = dir.join("automations");
        fs::create_dir_all(&automations).unwrap();
        fs::write(automations.join("rules.sealed"), b"sealed").unwrap();
        fs::write(automations.join("key"), b"k").unwrap();
        let path = dir.join("config.json");
        let mut json: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        json["settings"]["run_commands"] =
            serde_json::json!([{"id": "a", "name": "Deploy", "command": "make"}]);
        json["settings"]["call_audio"] = true.into();
        json["settings"]["call_audio_devices"] = serde_json::json!({"p": "AA:BB"});
        json["settings"]["stay_awake"] = true.into();
        fs::write(&path, serde_json::to_vec(&json).unwrap()).unwrap();
        let before = fs::read(dir.join("certificate.pem")).unwrap();

        let reloaded = Config::load_from_dir(dir.clone()).unwrap();
        assert!(!automations.exists(), "the rules and their key are gone");
        let written = fs::read_to_string(&path).unwrap();
        for gone in ["run_commands", "call_audio"] {
            assert!(!written.contains(gone), "{gone} still in config.json");
        }
        assert_eq!(reloaded.stored.device_id, device_id, "same device id");
        assert!(
            reloaded.stored.trusted_devices.contains_key(&id),
            "pairing kept"
        );
        assert!(
            reloaded.stored.settings.stay_awake,
            "other preferences kept"
        );
        assert_eq!(fs::read(dir.join("certificate.pem")).unwrap(), before);
        // A second start has nothing to remove and changes nothing.
        assert!(!remove_stored_removed_features(&dir));
        let _ = fs::remove_dir_all(dir);
    }

    /// A phone that pairs under a new id with the same name and type is only
    /// a *candidate* for replacing the older record: nothing is deleted by
    /// the trust store itself (review finding C).
    #[test]
    fn pairing_a_lookalike_queues_a_candidate_and_deletes_nothing() {
        let dir = std::env::temp_dir().join(format!("desklink-replace-{}", Uuid::new_v4()));
        let mut config = Config::load_from_dir(dir.clone()).unwrap();
        let info = |id: &str, name: &str, kind: &str| DeviceInfo {
            id: id.to_string(),
            name: name.to_string(),
            device_type: kind.to_string(),
            protocol_version: 9,
            incoming_capabilities: Vec::new(),
            outgoing_capabilities: Vec::new(),
            instance_id: None,
        };
        let tag = Uuid::new_v4().simple().to_string();
        let (old, fresh) = (format!("old-{tag}"), format!("new-{tag}"));
        let (other, tablet) = (format!("other-{tag}"), format!("tablet-{tag}"));
        let name = format!("Pixel {tag}");
        config
            .trust_device(&info(&old, &name, "phone"), "cert-old".into())
            .unwrap();
        config
            .trust_device(&info(&other, "Another phone", "phone"), "cert-o".into())
            .unwrap();
        config
            .trust_device(&info(&tablet, &name, "tablet"), "cert-t".into())
            .unwrap();
        let _ = take_replacement_candidates();
        // Re-confirming the same id (even with a new certificate) is nothing.
        config
            .trust_device(&info(&old, &name, "phone"), "cert-old-2".into())
            .unwrap();
        // (The queue is the process's: another test may have queued its own.)
        let ours = |candidates: Vec<ReplacementCandidate>| {
            candidates
                .into_iter()
                .filter(|candidate| candidate.name == name)
                .count()
        };
        assert_eq!(ours(take_replacement_candidates()), 0);
        // The lookalike: new id, same name and type.
        config
            .trust_device(&info(&fresh, &name, "phone"), "cert-new".into())
            .unwrap();
        assert!(config.is_trusted(&old), "not deleted here");
        assert!(config.is_trusted(&fresh));
        let mine: Vec<_> = take_replacement_candidates()
            .into_iter()
            .filter(|candidate| candidate.new_id == fresh)
            .collect();
        assert_eq!(
            mine,
            [ReplacementCandidate {
                old_id: old.clone(),
                new_id: fresh.clone(),
                name: name.clone(),
            }],
            "only the same name and type"
        );
        assert_eq!(ours(take_replacement_candidates()), 0, "taken once");
        let _ = fs::remove_dir_all(dir);
    }

    /// Settings -> Calls: the switch writes `pause_media_on_call` (what
    /// `DaemonCommand::SetPauseMediaOnCall` does) and it survives a restart.
    #[test]
    fn the_calls_switch_for_pausing_music_is_saved() {
        let dir = std::env::temp_dir().join(format!("desklink-pause-{}", Uuid::new_v4()));
        let mut config = Config::load_from_dir(dir.clone()).unwrap();
        assert!(config.stored.settings.pause_media_on_call, "on by default");
        config
            .update_settings(|settings| settings.pause_media_on_call = false)
            .unwrap();
        drop(config);
        let reloaded = Config::load_from_dir(dir.clone()).unwrap();
        assert!(!reloaded.stored.settings.pause_media_on_call);
        let _ = fs::remove_dir_all(dir);
    }

    /// "Pause music during calls" is on unless turned off, also for a
    /// config written before the switch existed.
    #[test]
    fn pausing_music_during_calls_defaults_on() {
        assert!(AppSettings::default().pause_media_on_call);
        let old: AppSettings = serde_json::from_str(r#"{"stay_awake": true}"#).unwrap();
        assert!(old.pause_media_on_call);
        let off: AppSettings = serde_json::from_str(r#"{"pause_media_on_call": false}"#).unwrap();
        assert!(!off.pause_media_on_call);
    }

    /// Settings default off, survive a restart, and load from a config that
    /// predates them. (The process-wide mirror is shared by every test that
    /// loads a config, so this checks what was persisted, not the mirror.)
    #[test]
    fn settings_persist_and_default_off_for_configs_that_predate_them() {
        let dir = std::env::temp_dir().join(format!("desklink-settings-{}", Uuid::new_v4()));
        let mut config = Config::load_from_dir(dir.clone()).unwrap();
        assert!(!config.stored.settings.stay_awake);
        config
            .update_settings(|settings| settings.stay_awake = true)
            .unwrap();
        drop(config);
        assert!(
            Config::load_from_dir(dir.clone())
                .unwrap()
                .stored
                .settings
                .stay_awake
        );

        let path = dir.join("config.json");
        let mut json: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        json.as_object_mut().unwrap().remove("settings");
        fs::write(&path, serde_json::to_vec(&json).unwrap()).unwrap();
        assert!(
            !Config::load_from_dir(dir.clone())
                .unwrap()
                .stored
                .settings
                .stay_awake
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn terminal_access_persists_across_a_restart_and_can_be_revoked() {
        let dir = std::env::temp_dir().join(format!("desklink-term-persist-{}", Uuid::new_v4()));
        let id = format!("phone-{}", Uuid::new_v4().simple());
        let mut config = trusted_config(&dir, &id, "cert-a");

        config.set_terminal_access(&id, true).unwrap();
        assert!(terminal_access_allowed(&id));
        drop(config);

        // Simulate a fresh process: the mirror forgets, the file remembers.
        mirror_terminal_access(&id, false);
        let mut reloaded = Config::load_from_dir(dir.clone()).unwrap();
        assert!(terminal_access_allowed(&id), "consent was not persisted");

        reloaded.set_terminal_access(&id, false).unwrap();
        assert!(!terminal_access_allowed(&id));
        drop(reloaded);
        let again = Config::load_from_dir(dir.clone()).unwrap();
        assert!(!again.stored.trusted_devices[&id].terminal_access);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn terminal_access_cannot_be_granted_to_an_unpaired_device() {
        let dir = std::env::temp_dir().join(format!("desklink-term-unpaired-{}", Uuid::new_v4()));
        let mut config = Config::load_from_dir(dir.clone()).unwrap();
        assert!(config.set_terminal_access("never-paired", true).is_err());
        assert!(!terminal_access_allowed("never-paired"));
        let _ = fs::remove_dir_all(dir);
    }

    /// A remembered desktop grant follows the pairing exactly like terminal
    /// consent: kept on re-confirmation, gone on unpair or a new certificate.
    #[test]
    fn a_remembered_desktop_grant_belongs_to_the_pairing() {
        let dir = std::env::temp_dir().join(format!("desklink-rd-grant-{}", Uuid::new_v4()));
        let id = format!("phone-{}", Uuid::new_v4().simple());
        let info = DeviceInfo::local(id.clone(), "Test phone".to_string());
        let mut config = trusted_config(&dir, &id, "cert-a");

        set_remote_desktop_token(&id, Some("t1"));
        set_remote_desktop_token(&id, Some("t2"));
        assert_eq!(
            remote_desktop_token(&id).as_deref(),
            Some("t2"),
            "a fresh token replaces the used one"
        );

        config.trust_device(&info, "cert-a".to_string()).unwrap();
        assert_eq!(
            remote_desktop_token(&id).as_deref(),
            Some("t2"),
            "re-confirming dropped the grant"
        );

        config.trust_device(&info, "cert-b".to_string()).unwrap();
        assert_eq!(
            remote_desktop_token(&id),
            None,
            "a new certificate inherited desktop control"
        );

        set_remote_desktop_token(&id, Some("t3"));
        config.untrust_device(&id).unwrap();
        assert_eq!(
            remote_desktop_token(&id),
            None,
            "unpairing kept desktop control"
        );
        let _ = fs::remove_dir_all(dir);
    }

    /// Re-confirming a pairing keeps the user's choice; unpairing, or a new
    /// certificate behind the same id, resets it to off.
    #[test]
    fn consent_survives_reconfirmation_but_not_unpairing_or_a_new_certificate() {
        let dir = std::env::temp_dir().join(format!("desklink-term-trust-{}", Uuid::new_v4()));
        let id = format!("phone-{}", Uuid::new_v4().simple());
        let info = DeviceInfo::local(id.clone(), "Test phone".to_string());
        let mut config = trusted_config(&dir, &id, "cert-a");
        config.set_terminal_access(&id, true).unwrap();

        config.trust_device(&info, "cert-a".to_string()).unwrap();
        assert!(
            terminal_access_allowed(&id),
            "re-confirming revoked consent"
        );

        config.trust_device(&info, "cert-b".to_string()).unwrap();
        assert!(
            !terminal_access_allowed(&id),
            "a new certificate inherited consent"
        );

        config.set_terminal_access(&id, true).unwrap();
        config.untrust_device(&id).unwrap();
        assert!(!terminal_access_allowed(&id));
        config.trust_device(&info, "cert-b".to_string()).unwrap();
        assert!(!terminal_access_allowed(&id), "a re-pair inherited consent");
        let _ = fs::remove_dir_all(dir);
    }

    /// Everything a fresh config creates is for this user only: no group or
    /// other bits on any file or directory (the session-password hash, every
    /// pairing certificate and the partial transfers live here).
    #[test]
    fn a_fresh_config_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir()
            .join(format!(
                "desklink-private-config-{}",
                uuid::Uuid::new_v4().simple()
            ))
            .join("protocol");
        let config = Config::load_from_dir(dir.clone()).unwrap();
        config.save().unwrap();
        fn walk(path: &std::path::Path, bad: &mut Vec<String>) {
            let mode = fs::symlink_metadata(path).unwrap().permissions().mode() & 0o777;
            if mode & 0o077 != 0 {
                bad.push(format!("{} {mode:o}", path.display()));
            }
            if path.is_dir() {
                for entry in fs::read_dir(path).unwrap().flatten() {
                    walk(&entry.path(), bad);
                }
            }
        }
        let mut bad = Vec::new();
        walk(&dir, &mut bad);
        assert!(bad.is_empty(), "{bad:?}");
        let _ = fs::remove_dir_all(dir.parent().unwrap());
    }

    /// A remote unpair (the phone said `pair:false`) forgets the revs the desk
    /// remembered, so the phone's snapshots are sent again after a re-pair:
    /// with a stale `have` the contacts stayed empty.
    #[test]
    fn untrusting_a_device_forgets_the_snapshot_revs_it_sent() {
        use crate::device_links::packet::NetworkPacket;
        let dir = std::env::temp_dir().join(format!("desklink-forget-revs-{}", Uuid::new_v4()));
        let mut config = Config::load_from_dir(dir.clone()).unwrap();
        let id = format!("phone-{}", Uuid::new_v4().simple());
        let info = DeviceInfo {
            id: id.clone(),
            name: "Pixel".into(),
            device_type: "phone".into(),
            protocol_version: 9,
            incoming_capabilities: Vec::new(),
            outgoing_capabilities: Vec::new(),
            instance_id: None,
        };
        config.trust_device(&info, "cert-1".into()).unwrap();
        let mut snapshot = NetworkPacket::new("desklink.contacts");
        snapshot.set("rev", "R1");
        crate::device_links::sync::note_received(&id, Some("instance-a"), "contacts", &snapshot);
        let have = |id: &str| {
            crate::device_links::sync::have_packet(id, Some("instance-a"))
                .body
                .get("have")
                .and_then(|have| have.as_object())
                .is_some_and(|have| have.contains_key("contacts"))
        };
        assert!(have(&id), "remembered");
        config.untrust_device(&id).unwrap();
        assert!(!have(&id), "forgotten with the pairing");
        // A new identity behind the same id forgets too.
        config.trust_device(&info, "cert-1".into()).unwrap();
        crate::device_links::sync::note_received(&id, Some("instance-a"), "contacts", &snapshot);
        config.trust_device(&info, "cert-2".into()).unwrap();
        assert!(!have(&id));
        let _ = fs::remove_dir_all(dir);
    }

    /// A settings file damaged by something else is set aside (never deleted),
    /// DeskLink starts fresh and says so in words, and the file the next start
    /// reads is a good one.
    #[test]
    fn a_corrupt_settings_file_is_set_aside_and_explained() {
        let dir = std::env::temp_dir().join(format!("desklink-corrupt-{}", Uuid::new_v4()));
        let first = Config::load_from_dir(dir.clone()).unwrap();
        let id = first.local_device_id().to_string();
        drop(first);
        fs::write(dir.join("config.json"), b"{\"device_id\": \"abc\", \"trunc").unwrap();
        let mut config =
            Config::load_from_dir(dir.clone()).expect("a damaged file does not stop it");
        assert_ne!(config.local_device_id(), id, "a fresh identity");
        let notice = config.take_load_notice().expect("the person is told");
        assert!(
            notice.contains("damaged") && notice.contains("config.corrupt-"),
            "{notice}"
        );
        assert!(
            !notice.contains("EOF") && !notice.contains("serde"),
            "plain words: {notice}"
        );
        let kept: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("config.corrupt-")
            })
            .collect();
        assert_eq!(kept.len(), 1);
        assert_eq!(
            fs::read(kept[0].path()).unwrap(),
            b"{\"device_id\": \"abc\", \"trunc"
        );
        assert!(serde_json::from_slice::<serde_json::Value>(
            &fs::read(dir.join("config.json")).unwrap()
        )
        .is_ok());
        let _ = fs::remove_dir_all(dir);
    }

    /// The file has a version; fields this build does not know survive a save;
    /// a file from a newer build is read but never rewritten.
    #[test]
    fn the_settings_file_is_versioned_forward_compatible_and_never_downgraded() {
        let dir = std::env::temp_dir().join(format!("desklink-version-{}", Uuid::new_v4()));
        let config = Config::load_from_dir(dir.clone()).unwrap();
        drop(config);
        let path = dir.join("config.json");
        let read =
            || serde_json::from_slice::<serde_json::Value>(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(read()["version"], CONFIG_VERSION);
        // A field from a newer build, at the top and among the settings.
        let mut json = read();
        json["from_the_future"] = serde_json::json!({"a": 1});
        json["settings"]["glide_v2"] = serde_json::json!("keep me");
        fs::write(&path, serde_json::to_vec(&json).unwrap()).unwrap();
        let mut config = Config::load_from_dir(dir.clone()).unwrap();
        config.set_device_name_for_test("Renamed");
        config.save().unwrap();
        let after = read();
        assert_eq!(after["from_the_future"]["a"], 1, "kept");
        assert_eq!(after["settings"]["glide_v2"], "keep me", "kept");
        assert_eq!(after["device_name"], "Renamed");
        // A newer file is not touched at all.
        let mut newer = read();
        newer["version"] = (CONFIG_VERSION + 1).into();
        newer["device_name"] = "Newer".into();
        let bytes = serde_json::to_vec(&newer).unwrap();
        fs::write(&path, &bytes).unwrap();
        let mut config = Config::load_from_dir(dir.clone()).unwrap();
        assert!(config
            .take_load_notice()
            .is_some_and(|n| n.contains("newer DeskLink")));
        config.set_device_name_for_test("Changed here");
        config.save().unwrap();
        assert_eq!(fs::read(&path).unwrap(), bytes, "left exactly as it was");
        let _ = fs::remove_dir_all(dir);
    }

    /// A settings file that cannot be rewritten (a full or read-only disk)
    /// does not stop DeskLink from starting.
    #[test]
    fn a_failed_save_at_start_is_not_fatal() {
        use std::os::unix::fs::PermissionsExt;
        if unsafe { libc::geteuid() } == 0 {
            return; // root writes anywhere
        }
        let dir = std::env::temp_dir().join(format!("desklink-nosave-{}", Uuid::new_v4()));
        drop(Config::load_from_dir(dir.clone()).unwrap());
        // Different bytes than a save would write, so a save is wanted.
        let path = dir.join("config.json");
        let json: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        fs::write(&path, serde_json::to_vec(&json).unwrap()).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o500)).unwrap();
        let loaded = Config::load_from_dir(dir.clone());
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(loaded.is_ok(), "{:?}", loaded.err());
        let _ = fs::remove_dir_all(dir);
    }

    /// 1.3 cleans up after itself: the old restore-token file and the old
    /// transfer folder go, quietly, once.
    #[test]
    fn leftover_tokens_and_old_transfer_state_are_removed() {
        let dir = std::env::temp_dir().join(format!("desklink-left-{}", Uuid::new_v4()));
        fs::create_dir_all(dir.join("transfers")).unwrap();
        fs::write(dir.join("transfers/a.json"), b"{}").unwrap();
        fs::write(dir.join("remote_desktop_tokens.json"), b"{}").unwrap();
        assert!(remove_stored_removed_features(&dir));
        assert!(!dir.join("transfers").exists());
        assert!(!dir.join("remote_desktop_tokens.json").exists());
        assert!(!remove_stored_removed_features(&dir), "once");
        let _ = fs::remove_dir_all(dir);
    }

    /// The partials move to the state directory in one go and only when the
    /// new place is not there yet.
    #[test]
    fn transfer_partials_move_out_of_the_config_directory_once() {
        let base = std::env::temp_dir().join(format!("desklink-move-{}", Uuid::new_v4()));
        let (old, new) = (
            base.join("config/webrtc-transfers"),
            base.join("state/desklink/transfers"),
        );
        fs::create_dir_all(&old).unwrap();
        fs::write(old.join(".desklink-1.part"), b"partial").unwrap();
        migrate_transfer_state(&old, &new);
        assert!(!old.exists());
        assert_eq!(fs::read(new.join(".desklink-1.part")).unwrap(), b"partial");
        // The new place exists: an old folder that appears again is left.
        fs::create_dir_all(&old).unwrap();
        fs::write(old.join("later"), b"x").unwrap();
        migrate_transfer_state(&old, &new);
        assert!(old.join("later").exists());
        assert!(!new.join("later").exists());
        let _ = fs::remove_dir_all(base);
    }

    /// "Unlock with this phone" is gone with the pairing, and with the
    /// identity: a different certificate behind the same id starts from off.
    #[test]
    fn unlock_with_a_phone_does_not_survive_unpair_or_a_new_identity() {
        let dir = std::env::temp_dir().join(format!("desklink-unlock-{}", Uuid::new_v4()));
        let mut config = Config::load_from_dir(dir.clone()).unwrap();
        let id = format!("phone-{}", Uuid::new_v4().simple());
        let info = DeviceInfo::local(id.clone(), "Pixel".into());
        config.trust_device(&info, "cert-1".into()).unwrap();
        config.stored.settings.unlock_devices.insert(id.clone());
        config.trust_device(&info, "cert-1".into()).unwrap();
        assert!(
            config.stored.settings.unlocks_from(&id),
            "same identity keeps it"
        );
        config.trust_device(&info, "cert-2".into()).unwrap();
        assert!(
            !config.stored.settings.unlocks_from(&id),
            "a new identity starts from off"
        );
        config.stored.settings.unlock_devices.insert(id.clone());
        config.untrust_device(&id).unwrap();
        assert!(!config.stored.settings.unlocks_from(&id), "unpaired");
        let _ = fs::remove_dir_all(dir);
    }
}
