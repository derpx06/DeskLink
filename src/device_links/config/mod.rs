//! The desk's identity and trust store ([`Config`]): this device's key and
//! certificate, the paired devices, and the app settings, loaded and saved
//! under `~/.config/desklink/` (frozen location; no migration of pairing
//! records, ids, certificates or preferences).
//!
//! Layout: this file holds `Config` and its trust operations; `settings` the
//! [`AppSettings`] and the mirrors other threads read, `cleanup` what is
//! handled at load (removed-feature data, the transfer-state move), `files`
//! safe writes and key generation. No cycles. May depend on: `device_info`,
//! `private_fs`, `session_lock`, `relay_link`, `webrtc::ice`, `peers`, `sync`,
//! `contacts`, `limits`, openssl.

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

mod cleanup;
mod files;
mod settings;
use cleanup::*;
use files::*;
use settings::*;

// The crate-facing surface (everything else stays inside).
pub(crate) use cleanup::SAVE_FAILED;
pub(crate) use files::{desklink_config_root, write_private};
#[cfg(test)]
pub(crate) use settings::set_gallery_for_tests;
#[cfg(test)]
pub(crate) use settings::DEFAULT_RELAY_URL;
pub(crate) use settings::{
    app_settings, remote_desktop_token, set_remote_desktop_token, terminal_access_allowed,
    AppSettings,
};

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
    /// Fields a newer build wrote for this phone, kept as they were.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// The format of `config.json` this build writes. A build meeting a *newer*
/// file reads what it understands and does not rewrite it (a downgrade used to
/// erase every setting it did not know); every build keeps the fields it does
/// not know when it writes its own.
const CONFIG_VERSION: u32 = 1;

/// The words for a settings file this build cannot read because a newer
/// DeskLink wrote it.
const NEWER_UNREADABLE: &str =
    "This settings file is from a newer DeskLink; changes will not be saved. Update DeskLink, or move the file aside to start fresh.";

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
        // A file whose `version` is newer than ours and which we cannot read
        // is "newer", not "damaged": it is left exactly as it is, and this
        // run neither starts a fresh identity nor writes anything.
        let mut newer_unreadable = false;
        let mut kept_device_id = None;
        let parsed = if state_path.exists() {
            let bytes = fs::read(&state_path).map_err(|error| {
                format!(
                    "DeskLink could not read its settings file {}: {error}",
                    state_path.display()
                )
            })?;
            let raw: Option<serde_json::Value> = serde_json::from_slice(&bytes).ok();
            let raw_version = raw
                .as_ref()
                .and_then(|value| value.get("version"))
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            match serde_json::from_slice::<StoredConfig>(&bytes) {
                Ok(stored) => Some(stored),
                Err(_) if raw_version > u64::from(CONFIG_VERSION) => {
                    newer_unreadable = true;
                    kept_device_id = raw
                        .as_ref()
                        .and_then(|value| value.get("device_id"))
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string);
                    None
                }
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
        // Newer and readable: used as it is, its unknown fields kept and its
        // `version` written back unchanged (a newer build reads it again).
        let newer = newer_unreadable
            || parsed
                .as_ref()
                .is_some_and(|stored| stored.version > CONFIG_VERSION);
        let read_only = newer_unreadable;
        if newer_unreadable {
            notice = Some(NEWER_UNREADABLE.to_string());
        } else if newer {
            notice = Some(
                "These settings were written by a newer DeskLink. Settings this version does not know are kept as they are."
                    .to_string(),
            );
        }
        // A new identity (no file, or a damaged one) comes with its own key
        // and certificate: the certificate's name is the device id, and a
        // phone refuses a link whose certificate names another id. The old
        // ones are kept beside, never deleted.
        let fresh_identity = parsed.is_none() && !newer_unreadable;
        let mut stored = if let Some(stored) = parsed {
            stored
        } else {
            let device_id = kept_device_id
                .take()
                .filter(|id| !id.is_empty())
                .unwrap_or_else(|| Uuid::new_v4().simple().to_string());
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

        // The phone still holds the old identity of this computer under its
        // name; it refuses a link from the new one until that entry is gone.
        if fresh_identity {
            if let Some(notice) = notice.as_mut() {
                notice.push_str(&format!(
                    " On each phone, unpair \"{}\" in DeskLink and pair again.",
                    stored.device_name
                ));
            }
        }
        let key_path = base_dir.join("privateKey.pem");
        let cert_path = base_dir.join("certificate.pem");
        if fresh_identity {
            let stamp = now_millis();
            for path in [&key_path, &cert_path] {
                if path.exists() {
                    let name = path.file_name().unwrap_or_default().to_string_lossy();
                    let _ = fs::rename(path, base_dir.join(format!("{name}.old-{stamp}")));
                }
            }
        }
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
        if !newer {
            for gone in REMOVED_SETTING_KEYS {
                stored.settings.extra.remove(gone);
            }
        }
        for device in stored.trusted_devices.values() {
            mirror_terminal_access(&device.id, device.terminal_access);
        }
        mirror_settings(&stored.settings);
        // Features that were removed: what they stored goes, quietly. The
        // settings that went with them are dropped by the save below
        // (unknown keys are not kept); pairing records and the rest stay.
        if !newer {
            remove_stored_removed_features(&base_dir);
        }
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
        if !config.read_only {
            if let Err(error) = config.save() {
                crate::device_links::limits::log_limited(
                    "config-save",
                    &format!(
                        "Could not save the settings; carrying on with them in memory: {error}"
                    ),
                );
            }
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
            // Not "saved": the person is told, and every command that saves
            // reports it (a pairing made now would be lost on restart).
            return Err(NEWER_UNREADABLE.to_string());
        }
        // So the window can tell a failed save of the settings (a full or
        // read-only disk) from any other file error.
        self.write_files()
            .map_err(|error| format!("{SAVE_FAILED}{error}"))
    }

    fn write_files(&self) -> Result<(), String> {
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
    /// never move identities or pairing records. Only computes the paths:
    /// [`TransferStatePaths::prepare`] moves old state, and does file IO, so
    /// call it after the configuration lock is released.
    pub fn transfer_state_paths(&self) -> TransferStatePaths {
        let old = self.base_dir.join("webrtc-transfers");
        if self.base_dir != config_dir() {
            // a test or another install root: where it always was
            return TransferStatePaths {
                new: old.clone(),
                old,
            };
        }
        let new = dirs::state_dir()
            .or_else(dirs::data_local_dir)
            .map(|state| state.join("desklink").join("transfers"))
            .unwrap_or_else(|| old.clone());
        TransferStatePaths { old, new }
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
            // Files chosen for the old identity are not for the new one.
            crate::device_links::daemon::file_transfer::forget_waiting(&info.id);
            // Nor are the photos and the count of the old one.
            crate::device_links::gallery::forget(&info.id);
            // And what is held in memory for it: unlock nonces, the clipboard
            // sent, glide, Do Not Disturb, art, lane back-off, transfer
            // records. (Only what is safe under this lock: stopping live
            // shares waits on the main loop; the cleanup of an unpair does
            // that outside it.)
            crate::device_links::peers::forget_identity_state(&info.id);
            // What the old identity said it already held is not what a new
            // one has: its snapshots are sent again.
            crate::device_links::sync::forget_device(&info.id);
            // "Unlock with this phone" is a consent for that identity.
            let settings = &mut self.stored.settings;
            let cleared = [
                settings.unlock_devices.remove(&info.id),
                settings.walk_away_devices.remove(&info.id),
                settings.gallery_devices.remove(&info.id),
                settings.dnd_devices.remove(&info.id),
            ];
            if cleared.contains(&true) {
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
                extra: previous
                    .as_ref()
                    .map(|record| record.extra.clone())
                    .unwrap_or_default(),
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

    /// The same phone paired again under a new id: Glide onto it, if set,
    /// continues onto the new pairing on the same edge.
    pub fn move_glide(&mut self, old_id: &str, new_id: &str) -> Result<(), String> {
        match self.stored.settings.glide.clone() {
            Some(glide) if glide.device == old_id => self.update_settings(|settings| {
                settings.glide = Some(crate::device_links::glide::GlideSetting {
                    device: new_id.to_string(),
                    edge: glide.edge,
                });
            }),
            _ => Ok(()),
        }
    }

    #[cfg(test)]
    pub fn app_settings_for_tests(&self) -> AppSettings {
        self.stored.settings.clone()
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
        let settings = &mut self.stored.settings;
        let cleared = [
            settings.unlock_devices.remove(device_id),
            settings.walk_away_devices.remove(device_id),
            settings.gallery_devices.remove(device_id),
            settings.dnd_devices.remove(device_id),
            // What was chosen for that phone alone: not inherited by a later
            // phone under the same id.
            settings.hidden_notifications.remove(device_id),
            settings.muted_command_alerts.remove(device_id),
            settings.keep_awake_phones.remove(device_id),
            settings.contact_caller_names.remove(device_id),
        ];
        if cleared.contains(&true) {
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

#[cfg(test)]
mod tests;
