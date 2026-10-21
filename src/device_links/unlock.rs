//! Unlock with the phone: off unless the user turned it on here, and then
//! only for a phone on this network that approves each time.
//!
//! The desk hands out a one-time nonce (`desklink.unlock.request`) — when the
//! lock-screen button is pressed, or when the phone asks — and unlocks only
//! when the phone answers `desklink.lock.request {setLocked: false, nonce}`
//! and every check in [`check`] passes; the session is then unlocked
//! through logind (`super::logind`). Anything else is refused, with a
//! reason code the phone can show, and logged.

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long a nonce stays usable.
const NONCE_LIFETIME: Duration = Duration::from_secs(60);

/// Why an unlock was refused: the exact codes the phone receives in
/// `desklink.lock {unlockRefused}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refusal {
    PrefOff,
    NoNonce,
    Expired,
    WrongDevice,
    Replayed,
    NotLocked,
    NotLan,
    NotReady,
}

impl Refusal {
    pub(crate) const fn code(self) -> &'static str {
        match self {
            Self::PrefOff => "pref-off",
            Self::NoNonce => "no-nonce",
            Self::Expired => "expired",
            Self::WrongDevice => "wrong-device",
            Self::Replayed => "replayed",
            Self::NotLocked => "not-locked",
            Self::NotLan => "not-lan",
            Self::NotReady => "not-ready",
        }
    }
}

/// The state the desk is in when an approval arrives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Conditions {
    /// Preferences → "Unlock with <phone> nearby".
    pub(crate) allowed: bool,
    /// The desktop session is locked.
    pub(crate) locked: bool,
    /// The phone's WebRTC pair is a local one (same network or hotspot),
    /// never through a relay or TURN.
    pub(crate) local: bool,
    /// The phone's encrypted connection is up.
    pub(crate) ready: bool,
}

#[derive(Debug, Clone)]
struct Issued {
    nonce: String,
    device_id: String,
    at: Instant,
    used: bool,
}

static ISSUED: Mutex<Vec<Issued>> = Mutex::new(Vec::new());

/// Whether the desk may offer `device_id` an unlock now; the refusal if not.
pub(crate) fn may_offer(conditions: Conditions) -> Result<(), Refusal> {
    if !conditions.allowed {
        return Err(Refusal::PrefOff);
    }
    if !conditions.ready {
        return Err(Refusal::NotReady);
    }
    if !conditions.local {
        return Err(Refusal::NotLan);
    }
    if !conditions.locked {
        return Err(Refusal::NotLocked);
    }
    Ok(())
}

/// A fresh 128-bit nonce for `device_id`, valid once for a minute.
pub(crate) fn issue(device_id: &str) -> String {
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    if let Ok(mut issued) = ISSUED.lock() {
        let now = Instant::now();
        issued.retain(|old| now.duration_since(old.at) < NONCE_LIFETIME * 2);
        issued.push(Issued {
            nonce: nonce.clone(),
            device_id: device_id.to_string(),
            at: now,
            used: false,
        });
    }
    nonce
}

/// Checks an approval from `device_id` carrying `nonce`, consuming the nonce.
pub(crate) fn approve(
    device_id: &str,
    nonce: Option<&str>,
    conditions: Conditions,
) -> Result<(), Refusal> {
    let Ok(mut issued) = ISSUED.lock() else {
        return Err(Refusal::NoNonce);
    };
    check(&mut issued, device_id, nonce, Instant::now(), conditions)
}

/// The whole gate, in order. A nonce that matched is spent, whatever the
/// outcome: a refused approval cannot be replayed once the reason clears.
fn check(
    issued: &mut [Issued],
    device_id: &str,
    nonce: Option<&str>,
    now: Instant,
    conditions: Conditions,
) -> Result<(), Refusal> {
    if !conditions.allowed {
        return Err(Refusal::PrefOff);
    }
    let Some(nonce) = nonce.filter(|nonce| !nonce.is_empty()) else {
        return Err(Refusal::NoNonce);
    };
    let Some(entry) = issued.iter_mut().find(|entry| entry.nonce == nonce) else {
        return Err(Refusal::NoNonce);
    };
    if entry.used {
        return Err(Refusal::Replayed);
    }
    entry.used = true;
    if entry.device_id != device_id {
        return Err(Refusal::WrongDevice);
    }
    if now.duration_since(entry.at) >= NONCE_LIFETIME {
        return Err(Refusal::Expired);
    }
    may_offer(conditions)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_GOOD: Conditions = Conditions {
        allowed: true,
        locked: true,
        local: true,
        ready: true,
    };

    fn issued(device: &str, at: Instant) -> Vec<Issued> {
        vec![Issued {
            nonce: "n1".into(),
            device_id: device.into(),
            at,
            used: false,
        }]
    }

    #[test]
    fn a_fresh_nonce_from_the_right_phone_on_the_lan_unlocks_once() {
        let now = Instant::now();
        let mut nonces = issued("phone", now);
        assert_eq!(
            check(&mut nonces, "phone", Some("n1"), now, ALL_GOOD),
            Ok(())
        );
        assert_eq!(
            check(&mut nonces, "phone", Some("n1"), now, ALL_GOOD),
            Err(Refusal::Replayed),
            "a nonce is used once"
        );
    }

    #[test]
    fn every_gate_refuses_with_its_own_code() {
        let now = Instant::now();
        let refused = |device: &str, nonce: Option<&str>, at: Instant, conditions| {
            let mut nonces = issued("phone", at);
            check(&mut nonces, device, nonce, now, conditions).unwrap_err()
        };
        let off = Conditions {
            allowed: false,
            ..ALL_GOOD
        };
        assert_eq!(refused("phone", Some("n1"), now, off), Refusal::PrefOff);
        assert_eq!(refused("phone", None, now, ALL_GOOD), Refusal::NoNonce);
        assert_eq!(
            refused("phone", Some("forged"), now, ALL_GOOD),
            Refusal::NoNonce
        );
        assert_eq!(
            refused("tablet", Some("n1"), now, ALL_GOOD),
            Refusal::WrongDevice
        );
        let stale = now - NONCE_LIFETIME - Duration::from_secs(1);
        assert_eq!(
            refused("phone", Some("n1"), stale, ALL_GOOD),
            Refusal::Expired
        );
        let relay = Conditions {
            local: false,
            ..ALL_GOOD
        };
        assert_eq!(refused("phone", Some("n1"), now, relay), Refusal::NotLan);
        let open = Conditions {
            locked: false,
            ..ALL_GOOD
        };
        assert_eq!(refused("phone", Some("n1"), now, open), Refusal::NotLocked);
        let connecting = Conditions {
            ready: false,
            ..ALL_GOOD
        };
        assert_eq!(
            refused("phone", Some("n1"), now, connecting),
            Refusal::NotReady
        );
    }

    #[test]
    fn a_refused_approval_spends_its_nonce() {
        let now = Instant::now();
        let mut nonces = issued("phone", now);
        let relay = Conditions {
            local: false,
            ..ALL_GOOD
        };
        assert_eq!(
            check(&mut nonces, "phone", Some("n1"), now, relay),
            Err(Refusal::NotLan)
        );
        assert_eq!(
            check(&mut nonces, "phone", Some("n1"), now, ALL_GOOD),
            Err(Refusal::Replayed)
        );
    }

    #[test]
    fn issued_nonces_are_128_bit_hex_and_distinct() {
        let (a, b) = (issue("phone"), issue("phone"));
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
        assert_eq!(Refusal::NotLan.code(), "not-lan");
    }
}
