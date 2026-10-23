//! QR-code pairing: scan once, nothing to type or compare.
//!
//! The desktop shows `desklink://pair?v=1&id=..&fp=..&s=..&h=..`:
//!
//! * `fp` — SHA-256 of this desktop's certificate (DER). The phone refuses the
//!   TLS link unless the certificate it sees hashes to this, so the phone pins
//!   the desktop out of band (Syncthing-style device ids).
//! * `s` — a 32-byte one-time secret. The phone proves it scanned the code by
//!   sending `desklink.pair.qr` with an HMAC over both ids and both certificate
//!   fingerprints. The desktop computes the phone's fingerprint from the
//!   certificate the link actually presented, so a relaying attacker — who must
//!   present its own certificate — cannot produce a valid proof.
//! * `h` — a LAN address hint (`ip:port`) so the phone can dial without
//!   waiting for discovery; kept for older phones.
//! * `a` and `p` — every LAN IPv4 address of this computer (comma-separated,
//!   virtual interfaces left out) and its TCP port: a newer phone dials them
//!   all at once, with the pinned TLS check, and needs no discovery at all.
//!
//! Only one code is live at a time; it expires after 5 minutes, is consumed by
//! the first valid proof, and is burned after 3 bad ones.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use openssl::hash::{hash, MessageDigest};
use openssl::pkey::PKey;
use openssl::sign::Signer;

const LIFETIME: Duration = Duration::from_secs(300);
const MAX_FAILURES: u8 = 3;
const MAC_LABEL: &str = "desklink-qr-v1";

struct Pending {
    secret: [u8; 32],
    expires_at: Instant,
    failures: u8,
    uri: String,
}

static PENDING: Mutex<Option<Pending>> = Mutex::new(None);

/// The secret of the code that was last replaced, withdrawn or let expire, so
/// a phone that scans it afterwards is told it *expired* (and the person is
/// shown a new one) instead of "invalid". Only that one; only its secret.
static RETIRED: Mutex<Option<[u8; 32]>> = Mutex::new(None);

/// How many times a retired code was scanned and refused, for the pairing
/// page to take (see [`take_expired_notice`]).
static EXPIRED_SCANS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn retire(secret: [u8; 32]) {
    if let Ok(mut retired) = RETIRED.lock() {
        *retired = Some(secret);
    }
}

/// Whether a phone scanned a code that has expired since the last call: the
/// pairing page says so and shows the new code.
pub fn take_expired_notice() -> bool {
    EXPIRED_SCANS.swap(0, std::sync::atomic::Ordering::AcqRel) > 0
}

/// Whether `received` is the proof of the retired code.
fn matches_retired(
    received: &[u8],
    desktop_id: &str,
    phone_id: &str,
    desktop_fp: &[u8; 32],
    phone_fp: &[u8; 32],
) -> bool {
    let Some(secret) = RETIRED.lock().ok().and_then(|retired| *retired) else {
        return false;
    };
    proof(&secret, desktop_id, phone_id, desktop_fp, phone_fp).is_ok_and(|expected| {
        received.len() == expected.len() && openssl::memcmp::eq(received, &expected)
    })
}

/// Tests that use the one live code take this first: they share it.
#[cfg(test)]
pub(crate) static TEST_LOCK: Mutex<()> = Mutex::new(());

/// The live code's secret, for a test to compute a phone's proof.
#[cfg(test)]
pub(crate) fn live_secret() -> Option<[u8; 32]> {
    PENDING.lock().ok()?.as_ref().map(|pending| pending.secret)
}

/// SHA-256 of a DER certificate.
pub fn fingerprint(certificate_der: &[u8]) -> Result<[u8; 32], String> {
    let digest = hash(MessageDigest::sha256(), certificate_der).map_err(|e| e.to_string())?;
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    Ok(out)
}

/// Starts a new pairing code, replacing any previous one, and returns its URI.
pub fn start(
    desktop_id: &str,
    desktop_certificate_der: &[u8],
    host: Option<String>,
    addresses: &[std::net::Ipv4Addr],
    port: u16,
) -> Result<String, String> {
    let mut secret = [0u8; 32];
    openssl::rand::rand_bytes(&mut secret).map_err(|e| e.to_string())?;
    let fp = fingerprint(desktop_certificate_der)?;
    let mut uri = format!(
        "desklink://pair?v=1&id={desktop_id}&fp={}&s={}",
        URL_SAFE_NO_PAD.encode(fp),
        URL_SAFE_NO_PAD.encode(secret),
    );
    if let Some(host) = host {
        uri.push_str("&h=");
        uri.push_str(&host);
    }
    if !addresses.is_empty() {
        let list: Vec<String> = addresses.iter().map(ToString::to_string).collect();
        uri.push_str(&format!("&a={}&p={port}", list.join(",")));
    }
    let mut pending = PENDING
        .lock()
        .map_err(|_| "QR pairing state poisoned".to_string())?;
    // The code on screen is replaced: the old one is spent, and scanning it
    // is "expired".
    if let Some(old) = pending.as_ref() {
        retire(old.secret);
    }
    *pending = Some(Pending {
        secret,
        expires_at: Instant::now() + LIFETIME,
        failures: 0,
        uri: uri.clone(),
    });
    Ok(uri)
}

/// Withdraws the live code (the dialog was closed).
pub fn cancel() {
    if let Ok(mut pending) = PENDING.lock() {
        if let Some(old) = pending.as_ref() {
            retire(old.secret);
        }
        *pending = None;
    }
}

/// The URI of the live, unexpired code, for the UI to render.
pub fn current_uri() -> Option<String> {
    let pending = PENDING.lock().ok()?;
    pending
        .as_ref()
        .filter(|pending| pending.expires_at > Instant::now())
        .map(|pending| pending.uri.clone())
}

/// The proof both apps compute: HMAC-SHA256(secret, label‖ids‖fingerprints).
/// Mirrored by `QrPairing.proof` on Android — keep the message identical.
pub fn proof(
    secret: &[u8],
    desktop_id: &str,
    phone_id: &str,
    desktop_fp: &[u8; 32],
    phone_fp: &[u8; 32],
) -> Result<Vec<u8>, String> {
    let message = format!(
        "{MAC_LABEL}\n{desktop_id}\n{phone_id}\n{}\n{}",
        hex(desktop_fp),
        hex(phone_fp)
    );
    let key = PKey::hmac(secret).map_err(|e| e.to_string())?;
    let mut signer = Signer::new(MessageDigest::sha256(), &key).map_err(|e| e.to_string())?;
    signer
        .update(message.as_bytes())
        .map_err(|e| e.to_string())?;
    signer.sign_to_vec().map_err(|e| e.to_string())
}

/// Checks a phone's proof against the live code. Success consumes the code;
/// each failure counts, and the code is burned after `MAX_FAILURES`.
pub fn verify(
    desktop_id: &str,
    phone_id: &str,
    desktop_fp: &[u8; 32],
    phone_fp: &[u8; 32],
    mac_b64: &str,
) -> Result<(), String> {
    let mut pending = PENDING
        .lock()
        .map_err(|_| "QR pairing state poisoned".to_string())?;
    let expired = || {
        EXPIRED_SCANS.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        Err("The QR pairing code expired".to_string())
    };
    let received = URL_SAFE_NO_PAD
        .decode(mac_b64.trim_end_matches('='))
        .map_err(|_| "Malformed QR pairing proof".to_string())?;
    let Some(live) = pending.as_mut() else {
        // Nothing is live: was the code that was, scanned late?
        if matches_retired(&received, desktop_id, phone_id, desktop_fp, phone_fp) {
            return expired();
        }
        return Err("No QR pairing code is active".to_string());
    };
    if live.expires_at <= Instant::now() {
        retire(live.secret);
        *pending = None;
        return expired();
    }
    let expected = proof(&live.secret, desktop_id, phone_id, desktop_fp, phone_fp)?;
    if received.len() == expected.len() && openssl::memcmp::eq(&received, &expected) {
        *pending = None;
        return Ok(());
    }
    // Not the live code: perhaps the one that was replaced a moment ago.
    if matches_retired(&received, desktop_id, phone_id, desktop_fp, phone_fp) {
        return expired();
    }
    live.failures += 1;
    if live.failures >= MAX_FAILURES {
        *pending = None;
    }
    Err("Invalid QR pairing proof".to_string())
}

/// Tests: the live code runs out now.
#[cfg(test)]
pub(crate) fn expire_live_for_test() {
    if let Ok(mut pending) = PENDING.lock() {
        if let Some(live) = pending.as_mut() {
            live.expires_at = Instant::now() - Duration::from_secs(1);
        }
    }
}

/// QR modules as rows of booleans (true = dark), for the UI to rasterise.
pub fn modules(uri: &str) -> Result<(usize, Vec<bool>), String> {
    let code = qrcode::QrCode::new(uri.as_bytes()).map_err(|e| e.to_string())?;
    let width = code.width();
    let dark = code
        .to_colors()
        .into_iter()
        .map(|color| color == qrcode::Color::Dark)
        .collect();
    Ok((width, dark))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pinned on Android too (`QrPairingTest`): both apps must compute the
    /// same bytes, or QR pairing silently fails.
    #[test]
    fn the_proof_matches_the_shared_cross_platform_vector() {
        let proof = proof(&[7u8; 32], "desk", "phone", &[1u8; 32], &[2u8; 32]).unwrap();
        assert_eq!(
            hex(&proof),
            "bd84b158ada359358ce90dce45c314f15be4b48f00f8a96aae82b12769f9fc41"
        );
    }

    // The tests share the one global code, so they run as one sequence.
    #[test]
    fn a_code_pairs_once_rejects_forgeries_and_burns_after_three_failures() {
        let _one_code = TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let desktop_fp = [1u8; 32];
        let phone_fp = [2u8; 32];

        let addresses = ["192.0.2.1".parse().unwrap(), "10.0.0.5".parse().unwrap()];
        let uri = start(
            "desk",
            b"desktop-cert-der",
            Some("192.0.2.1:1717".into()),
            &addresses,
            1717,
        )
        .unwrap();
        assert!(uri.starts_with("desklink://pair?v=1&id=desk&fp="));
        assert!(
            uri.ends_with("&h=192.0.2.1:1717&a=192.0.2.1,10.0.0.5&p=1717"),
            "{uri}"
        );
        assert_eq!(current_uri().as_deref(), Some(uri.as_str()));
        let secret = PENDING.lock().unwrap().as_ref().unwrap().secret;
        let good = URL_SAFE_NO_PAD
            .encode(proof(&secret, "desk", "phone", &desktop_fp, &phone_fp).unwrap());

        // A relay presents its own certificate, so the fingerprint differs.
        let relayed = verify("desk", "phone", &desktop_fp, &[9u8; 32], &good);
        assert!(relayed.is_err());
        assert!(verify("desk", "phone", &desktop_fp, &phone_fp, &good).is_ok());
        // Single use.
        assert!(verify("desk", "phone", &desktop_fp, &phone_fp, &good).is_err());
        assert_eq!(current_uri(), None);

        let without = start("desk", b"desktop-cert-der", None, &[], 1716).unwrap();
        assert!(
            !without.contains("&a=") && !without.contains("&p="),
            "no addresses: no list"
        );
        for _ in 0..MAX_FAILURES {
            assert!(verify("desk", "phone", &desktop_fp, &phone_fp, "AAAA").is_err());
        }
        // Burned: even the right proof for the burned secret no longer works.
        assert_eq!(current_uri(), None);

        let (width, dark) = modules("desklink://pair?v=1").unwrap();
        assert_eq!(dark.len(), width * width);
        cancel();
    }

    /// A phone that scans a code that has since expired (timed out, replaced
    /// by a newer one, withdrawn) is refused as *expired*, once, so the pairing
    /// page can say so and show the new code; a wrong proof is still just
    /// invalid and says nothing.
    #[test]
    fn scanning_an_expired_code_is_told_apart_and_noticed_once() {
        let _one_code = TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let (desktop_fp, phone_fp) = ([1u8; 32], [2u8; 32]);
        let proof_for = |secret: [u8; 32]| {
            URL_SAFE_NO_PAD.encode(proof(&secret, "desk", "phone", &desktop_fp, &phone_fp).unwrap())
        };
        take_expired_notice();
        // Timed out.
        start("desk", b"cert", None, &[], 1716).unwrap();
        let old = live_secret().unwrap();
        expire_live_for_test();
        assert_eq!(
            verify("desk", "phone", &desktop_fp, &phone_fp, &proof_for(old)),
            Err("The QR pairing code expired".to_string())
        );
        assert!(take_expired_notice(), "the page is told");
        assert!(!take_expired_notice(), "once");
        // Replaced by a newer code (the page showed a fresh one).
        start("desk", b"cert", None, &[], 1716).unwrap();
        let older = live_secret().unwrap();
        start("desk", b"cert", None, &[], 1716).unwrap();
        assert_eq!(
            verify("desk", "phone", &desktop_fp, &phone_fp, &proof_for(older)),
            Err("The QR pairing code expired".to_string())
        );
        assert!(take_expired_notice());
        assert!(current_uri().is_some(), "the new code is still live");
        // A forgery is invalid, not expired, and not announced.
        assert_eq!(
            verify(
                "desk",
                "phone",
                &desktop_fp,
                &phone_fp,
                &proof_for([9u8; 32])
            ),
            Err("Invalid QR pairing proof".to_string())
        );
        assert!(!take_expired_notice());
        // Withdrawn (the dialog closed), then scanned.
        let live = live_secret().unwrap();
        cancel();
        assert_eq!(
            verify("desk", "phone", &desktop_fp, &phone_fp, &proof_for(live)),
            Err("The QR pairing code expired".to_string())
        );
        assert!(take_expired_notice());
        // Something never shown is just not active.
        assert_eq!(
            verify(
                "desk",
                "phone",
                &desktop_fp,
                &phone_fp,
                &proof_for([5u8; 32])
            ),
            Err("No QR pairing code is active".to_string())
        );
        assert!(!take_expired_notice());
        cancel();
    }
}
