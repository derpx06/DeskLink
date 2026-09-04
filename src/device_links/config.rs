use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

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

        let config = Self {
            base_dir,
            stored,
            key,
            certificate,
        };
        config.save()?;
        Ok(config)
    }

    pub fn save(&self) -> Result<(), String> {
        fs::create_dir_all(&self.base_dir).map_err(|err| err.to_string())?;
        fs::write(
            self.base_dir.join("config.json"),
            serde_json::to_vec_pretty(&self.stored).map_err(|err| err.to_string())?,
        )
        .map_err(|err| err.to_string())?;
        write_private_key(
            &self.base_dir.join("privateKey.pem"),
            &self
                .key
                .private_key_to_pem_pkcs8()
                .map_err(|err| err.to_string())?,
        )?;
        fs::write(
            self.base_dir.join("certificate.pem"),
            self.certificate.to_pem().map_err(|err| err.to_string())?,
        )
        .map_err(|err| err.to_string())?;
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
        let stun = std::env::var("DESKLINK_STUN_SERVERS")
            .unwrap_or_else(|_| self.stored.ice_stun_servers.join(","));
        let turn = std::env::var("DESKLINK_TURN_SERVERS")
            .unwrap_or_else(|_| self.stored.ice_turn_servers.join(","));
        match crate::device_links::webrtc::ice::IceServerConfig::parse(&stun, &turn) {
            Ok(config) => config,
            Err(error) => {
                eprintln!("[DeskLink] Ignoring the configured ICE servers: {error}");
                crate::device_links::webrtc::ice::IceServerConfig::default()
            }
        }
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

    pub fn trust_device(
        &mut self,
        info: &DeviceInfo,
        certificate_pem: String,
    ) -> Result<(), String> {
        self.stored.trusted_devices.insert(
            info.id.clone(),
            TrustedDevice {
                id: info.id.clone(),
                name: info.name.clone(),
                device_type: info.device_type.clone(),
                protocol_version: info.protocol_version,
                certificate_pem,
            },
        );
        self.save()
    }

    pub fn untrust_device(&mut self, device_id: &str) -> Result<(), String> {
        self.stored.trusted_devices.remove(device_id);
        self.save()
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
fn write_private_key(path: &std::path::Path, pem: &[u8]) -> Result<(), String> {
    use std::io::Write;

    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(|err| err.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // An existing file keeps its old mode when reopened, so tighten it too.
        let _ = file.set_permissions(fs::Permissions::from_mode(0o600));
    }
    file.write_all(pem).map_err(|err| err.to_string())?;
    file.sync_all().map_err(|err| err.to_string())
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
}
