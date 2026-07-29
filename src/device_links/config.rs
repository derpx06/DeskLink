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
    /// Portal restore tokens are revocable desktop-consent references, never
    /// pairing credentials. They are keyed by the paired device ID so a
    /// replacement WebRTC generation can request the same user-approved
    /// RemoteDesktop session without creating a prompt loop.
    #[serde(default)]
    remote_desktop_restore_tokens: HashMap<String, String>,
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
                remote_desktop_restore_tokens: HashMap::new(),
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
        let certificate = if cert_path.exists() {
            X509::from_pem(&fs::read(&cert_path).map_err(|err| err.to_string())?)
                .map_err(|err| err.to_string())?
        } else {
            generate_certificate(&key, &stored.device_id).map_err(|err| err.to_string())?
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
        fs::write(
            self.base_dir.join("privateKey.pem"),
            self.key
                .private_key_to_pem_pkcs8()
                .map_err(|err| err.to_string())?,
        )
        .map_err(|err| err.to_string())?;
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
        self.stored.remote_desktop_restore_tokens.remove(device_id);
        self.save()
    }

    pub fn remote_desktop_restore_token(&self, device_id: &str) -> Option<String> {
        self.stored.remote_desktop_restore_tokens.get(device_id).cloned()
    }

    pub fn set_remote_desktop_restore_token(
        &mut self,
        device_id: impl Into<String>,
        token: Option<String>,
    ) -> Result<(), String> {
        let device_id = device_id.into();
        match token.filter(|value| !value.is_empty()) {
            Some(token) => {
                self.stored.remote_desktop_restore_tokens.insert(device_id, token);
            }
            None => {
                self.stored.remote_desktop_restore_tokens.remove(&device_id);
            }
        }
        self.save()
    }
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
}
