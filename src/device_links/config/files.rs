//! Writing config files safely (atomic, private, only when changed), the
//! config root, and generating this device's key and certificate.
//!
//! Sibling edges: none.

use super::*;

/// Confirms the stored certificate actually certifies the stored private key.
/// Comparing the public halves in DER is exact and avoids a signature check.
pub(super) fn certificate_matches_key(certificate: &X509, key: &PKey<Private>) -> bool {
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
pub(super) fn write_atomic_if_changed(
    path: &std::path::Path,
    bytes: &[u8],
    mode: u32,
) -> Result<(), String> {
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

    // One writer at a time: the temp name is fixed per path, so two threads saving the same file (the vault's
    // writer and an inline overflow save) interleaved their bytes into a file that was then renamed into place.
    // A leaf lock: nothing else is acquired while it is held (only the open, write, fsync and rename of one
    // small file), so a caller that holds the Config mutex or the vault's cannot deadlock on it; the wait is
    // another writer's fsync.
    static WRITING: Mutex<()> = Mutex::new(());
    let _one_at_a_time = WRITING
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
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

pub(super) fn config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("desklink")
        .join("protocol")
}

pub(super) fn generate_key() -> Result<PKey<Private>, openssl::error::ErrorStack> {
    let group = EcGroup::from_curve_name(Nid::X9_62_PRIME256V1)?;
    let key = EcKey::generate(&group)?;
    PKey::from_ec_key(key)
}

pub(super) fn generate_certificate(
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
