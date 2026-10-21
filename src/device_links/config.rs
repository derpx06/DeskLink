use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;
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

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredConfig {
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
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            stay_awake: false,
            session_password: None,
            relay_enabled: true,
            relay_url: String::new(),
            use_turn: true,
            start_at_login: None,
            shell_extension_prompt_dismissed: false,
            clipboard_sync: true,
            hidden_notifications: Default::default(),
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

pub struct Config {
    base_dir: PathBuf,
    stored: StoredConfig,
    key: PKey<Private>,
    certificate: X509,
}

impl Config {
    pub fn load() -> Result<Self, String> {
        Self::load_from_dir(config_dir())
    }

    pub fn load_from_dir(base_dir: PathBuf) -> Result<Self, String> {
        fs::create_dir_all(&base_dir).map_err(|err| err.to_string())?;
        let state_path = base_dir.join("config.json");
        let stored = if state_path.exists() {
            let bytes = fs::read(&state_path).map_err(|err| err.to_string())?;
            serde_json::from_slice(&bytes).map_err(|err| err.to_string())?
        } else {
            let device_id = Uuid::new_v4().simple().to_string();
            let device_name = hostname::get()
                .ok()
                .and_then(|name| name.into_string().ok())
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| "DeskLink".to_string());
            StoredConfig {
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

        for device in stored.trusted_devices.values() {
            mirror_terminal_access(&device.id, device.terminal_access);
        }
        mirror_settings(&stored.settings);
        let config = Self {
            base_dir,
            stored,
            key,
            certificate,
        };
        config.save()?;
        Ok(config)
    }

    /// Every write is atomic (temp file, fsync, rename, directory fsync), and
    /// the key and certificate are only written when they differ from disk.
    /// `save` runs on every pair and unpair; truncating in place meant a crash
    /// mid-write left a half-written config.json or private key, `load` then
    /// failed, and DeskLink never started again without losing every pairing.
    pub fn save(&self) -> Result<(), String> {
        fs::create_dir_all(&self.base_dir).map_err(|err| err.to_string())?;
        write_atomic(
            &self.base_dir.join("config.json"),
            &serde_json::to_vec_pretty(&self.stored).map_err(|err| err.to_string())?,
            0o644,
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
            0o644,
        )?;
        Ok(())
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
        self.base_dir.join("webrtc-transfers")
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
            // A new identity does not inherit remote control of this desktop.
            set_remote_desktop_token(&info.id, None);
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
        self.save_or_restore(&info.id, previous)?;
        if !same_identity {
            // The relay's allow list is built from the trust store.
            crate::device_links::relay_link::client::reset();
        }
        Ok(())
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
        // Remote control of this desktop is a property of the pairing too.
        set_remote_desktop_token(device_id, None);
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
}
