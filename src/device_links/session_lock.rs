//! The optional session password: a second factor for screen and terminal
//! access, on top of pairing.
//!
//! Set at the desk (Preferences), stored only as a salted PBKDF2-HMAC-SHA256
//! hash. A phone proves it once per connection: the password rides as an
//! optional field on the remote-session and terminal requests it already
//! sends, so no new packet type exists and the feature profile both apps must
//! agree on is unchanged. A connection is identified by its WebRTC
//! `session_id` (derived from the attempt's UUID, fresh for every peer the two
//! sides build), so a reconnect — a Wi-Fi/5G switch, a phone restart — asks
//! again (the phone keeps the password in memory and resends it). The
//! per-attempt `generation` is not used: it is the constant 1.
//!
//! Wrong guesses lock the device out: 5 failures, then 60 s, doubling to an
//! hour. The lockout is per device and survives reconnects, so reconnecting is
//! not a way around it.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use base64::Engine;
use serde::{Deserialize, Serialize};

const ITERATIONS: u32 = 600_000;
const FREE_ATTEMPTS: u32 = 5;
const FIRST_LOCKOUT: Duration = Duration::from_secs(60);
const MAX_LOCKOUT: Duration = Duration::from_secs(60 * 60);
/// Passwords longer than this are refused before hashing (PBKDF2 cost is
/// per byte only marginally, but nothing legitimate is this long).
pub const MAX_PASSWORD_BYTES: usize = 256;

/// Denial reasons sent to the phone. Stable wire values.
pub const PASSWORD_REQUIRED: &str = "password-required";
pub const PASSWORD_WRONG: &str = "password-wrong";
/// Followed by `:<seconds>` until the lockout ends.
pub const PASSWORD_LOCKED: &str = "password-locked";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PasswordHash {
    pub salt: String,
    pub hash: String,
    pub iterations: u32,
}

pub fn hash_password(password: &str) -> Result<PasswordHash, String> {
    let mut salt = [0u8; 16];
    openssl::rand::rand_bytes(&mut salt).map_err(|error| error.to_string())?;
    let hash = derive(password, &salt, ITERATIONS)?;
    let b64 = base64::engine::general_purpose::STANDARD;
    Ok(PasswordHash {
        salt: b64.encode(salt),
        hash: b64.encode(hash),
        iterations: ITERATIONS,
    })
}

fn derive(password: &str, salt: &[u8], iterations: u32) -> Result<[u8; 32], String> {
    let mut out = [0u8; 32];
    openssl::pkcs5::pbkdf2_hmac(
        password.as_bytes(),
        salt,
        iterations as usize,
        openssl::hash::MessageDigest::sha256(),
        &mut out,
    )
    .map_err(|error| error.to_string())?;
    Ok(out)
}

fn matches(password: &str, stored: &PasswordHash) -> bool {
    let b64 = base64::engine::general_purpose::STANDARD;
    let (Ok(salt), Ok(expected)) = (b64.decode(&stored.salt), b64.decode(&stored.hash)) else {
        return false;
    };
    derive(password, &salt, stored.iterations)
        .is_ok_and(|actual| openssl::memcmp::eq(&actual, &expected))
}

#[derive(Default)]
struct DeviceState {
    /// The connection (`wire.session_id`) this device proved the password on.
    unlocked_session: Option<u64>,
    failures: u32,
    locked_until: Option<Instant>,
    /// SHA-256 of the last wrong password counted. The same wrong password
    /// offered again (a reconnect resending a stale one on several requests
    /// at once) is refused but not counted twice.
    last_wrong: Option<[u8; 32]>,
}

/// What [`SessionLock::begin`] decided without hashing.
pub enum Precheck {
    Allowed,
    Denied(String),
    /// Hash `offered` (outside any lock) and report with [`SessionLock::finish`].
    /// Carries the lock's epoch: if an unpair or a password change happens
    /// while hashing, the answer no longer counts.
    Verify(u64),
}

#[derive(Default)]
pub struct SessionLock {
    devices: HashMap<String, DeviceState>,
    /// Bumped by every unlock-forgetting event (unpair, new identity,
    /// password change).
    epoch: u64,
}

fn digest(password: &str) -> [u8; 32] {
    let mut out = [0u8; 32];
    if let Ok(hash) =
        openssl::hash::hash(openssl::hash::MessageDigest::sha256(), password.as_bytes())
    {
        out.copy_from_slice(&hash);
    }
    out
}

impl SessionLock {
    /// Everything that needs no hashing: already unlocked, locked out, nothing
    /// offered, or the same wrong password again.
    pub fn begin(
        &mut self,
        stored: Option<&PasswordHash>,
        device_id: &str,
        session: u64,
        offered: Option<&str>,
        now: Instant,
    ) -> Precheck {
        if stored.is_none() {
            return Precheck::Allowed;
        }
        let state = self.devices.entry(device_id.to_string()).or_default();
        if state.unlocked_session == Some(session) {
            return Precheck::Allowed;
        }
        if let Some(until) = state.locked_until.filter(|until| *until > now) {
            return Precheck::Denied(format!(
                "{PASSWORD_LOCKED}:{}",
                (until - now).as_secs().max(1)
            ));
        }
        let Some(offered) = offered else {
            return Precheck::Denied(PASSWORD_REQUIRED.to_string());
        };
        if offered.len() > MAX_PASSWORD_BYTES {
            return Precheck::Denied(PASSWORD_WRONG.to_string());
        }
        if state.last_wrong == Some(digest(offered)) {
            return Precheck::Denied(PASSWORD_WRONG.to_string());
        }
        Precheck::Verify(self.epoch)
    }

    /// Records the outcome of hashing `offered`.
    pub fn finish(
        &mut self,
        device_id: &str,
        session: u64,
        offered: &str,
        correct: bool,
        epoch: u64,
        now: Instant,
    ) -> Result<(), String> {
        let changed = epoch != self.epoch;
        let state = self.devices.entry(device_id.to_string()).or_default();
        // A right answer to a question asked before the password changed or
        // the phone was unpaired unlocks nothing; nor does one that arrives
        // while a lockout (started by a concurrent wrong guess) runs.
        if correct && (changed || state.locked_until.is_some_and(|until| until > now)) {
            return Err(PASSWORD_REQUIRED.to_string());
        }
        if correct {
            *state = DeviceState {
                unlocked_session: Some(session),
                ..DeviceState::default()
            };
            return Ok(());
        }
        let wrong = digest(offered);
        if state.last_wrong == Some(wrong) {
            return Err(PASSWORD_WRONG.to_string());
        }
        state.last_wrong = Some(wrong);
        state.failures += 1;
        if state.failures >= FREE_ATTEMPTS {
            let doublings = (state.failures - FREE_ATTEMPTS).min(6);
            let lockout = (FIRST_LOCKOUT * 2u32.pow(doublings)).min(MAX_LOCKOUT);
            state.locked_until = Some(now + lockout);
            return Err(format!("{PASSWORD_LOCKED}:{}", lockout.as_secs()));
        }
        Err(PASSWORD_WRONG.to_string())
    }

    /// [`Self::begin`] then, if needed, hashing and [`Self::finish`] — all
    /// under `&mut self`. For tests; production hashes outside the lock.
    #[cfg(test)]
    pub fn check(
        &mut self,
        stored: Option<&PasswordHash>,
        device_id: &str,
        session: u64,
        offered: Option<&str>,
        now: Instant,
    ) -> Result<(), String> {
        match self.begin(stored, device_id, session, offered, now) {
            Precheck::Allowed => Ok(()),
            Precheck::Denied(reason) => Err(reason),
            Precheck::Verify(epoch) => {
                let offered = offered.unwrap_or_default();
                let correct = stored.is_some_and(|stored| matches(offered, stored));
                self.finish(device_id, session, offered, correct, epoch, now)
            }
        }
    }

    /// Forgets `device_id`'s unlock (unpaired, or paired again under a new
    /// identity). Its failures and lockout stay: they are a property of the
    /// guessing, not of the connection.
    pub fn forget_unlock(&mut self, device_id: &str) {
        self.epoch += 1;
        if let Some(state) = self.devices.get_mut(device_id) {
            state.unlocked_session = None;
            state.last_wrong = None;
        }
    }

    /// Forgets every unlock, e.g. when the password changes.
    pub fn relock_all(&mut self) {
        self.epoch += 1;
        for state in self.devices.values_mut() {
            state.unlocked_session = None;
            state.last_wrong = None;
        }
    }
}

/// `device_id` is no longer the phone that proved the password.
pub fn forget_unlock(device_id: &str) {
    if let Ok(mut lock) = shared().lock() {
        lock.forget_unlock(device_id);
    }
}

pub fn shared() -> &'static Mutex<SessionLock> {
    static LOCK: std::sync::OnceLock<Mutex<SessionLock>> = std::sync::OnceLock::new();
    LOCK.get_or_init(Default::default)
}

/// Whether `device_id` may act on connection `session` without offering
/// anything: no password is set, or it was already proved. Never counts as a
/// guess.
pub fn is_unlocked(device_id: &str, session: u64) -> bool {
    if crate::device_links::config::app_settings()
        .session_password
        .is_none()
    {
        return true;
    }
    shared().lock().is_ok_and(|lock| {
        lock.devices
            .get(device_id)
            .is_some_and(|state| state.unlocked_session == Some(session))
    })
}

/// Checks `device_id` against the current setting, logging failures. The
/// slow part — hashing — runs outside the shared lock, so one phone guessing
/// never stalls another phone's typing.
pub fn check(device_id: &str, session: u64, offered: Option<&str>) -> Result<(), String> {
    let stored = crate::device_links::config::app_settings().session_password;
    let precheck = shared()
        .lock()
        .map_err(|_| PASSWORD_REQUIRED.to_string())?
        .begin(stored.as_ref(), device_id, session, offered, Instant::now());
    let result = match precheck {
        Precheck::Allowed => return Ok(()),
        Precheck::Denied(reason) => Err(reason),
        Precheck::Verify(epoch) => {
            let offered = offered.unwrap_or_default();
            let correct = stored
                .as_ref()
                .is_some_and(|stored| matches(offered, stored));
            shared()
                .lock()
                .map_err(|_| PASSWORD_REQUIRED.to_string())?
                .finish(device_id, session, offered, correct, epoch, Instant::now())
        }
    };
    if result.is_err() && offered.is_some() {
        crate::device_links::connection_log::record(
            device_id,
            crate::device_links::connection_log::ConnectionEvent::PasswordRejected,
        );
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cheap(password: &str) -> PasswordHash {
        let salt = [7u8; 16];
        let b64 = base64::engine::general_purpose::STANDARD;
        PasswordHash {
            salt: b64.encode(salt),
            hash: b64.encode(derive(password, &salt, 1000).unwrap()),
            iterations: 1000,
        }
    }

    #[test]
    fn no_password_set_means_no_gate() {
        let mut lock = SessionLock::default();
        assert!(lock.check(None, "p", 1, None, Instant::now()).is_ok());
    }

    #[test]
    fn a_connection_unlocks_once_and_a_new_generation_asks_again() {
        let stored = cheap("hunter2");
        let mut lock = SessionLock::default();
        let now = Instant::now();
        assert_eq!(
            lock.check(Some(&stored), "p", 1, None, now),
            Err(PASSWORD_REQUIRED.into())
        );
        assert!(lock
            .check(Some(&stored), "p", 1, Some("hunter2"), now)
            .is_ok());
        assert!(lock.check(Some(&stored), "p", 1, None, now).is_ok());
        assert_eq!(
            lock.check(Some(&stored), "p", 2, None, now),
            Err(PASSWORD_REQUIRED.into())
        );
        assert!(lock.check(Some(&stored), "other", 1, None, now).is_err());
        lock.relock_all();
        assert!(lock.check(Some(&stored), "p", 1, None, now).is_err());
    }

    /// W7: every gate keys the proved password by the connection's
    /// `session_id` (fresh per peer). The per-attempt `generation` is the
    /// constant 1, so a gate that reverted to it would let one connection's
    /// proof carry to the next. Pinned at the five call sites.
    #[test]
    fn every_gate_passes_the_session_id_never_the_generation() {
        let sources = [
            (
                "coordinator.rs",
                include_str!("webrtc/coordinator.rs"),
                2usize,
            ),
            (
                "terminal_bridge.rs",
                include_str!("webrtc/terminal_bridge.rs"),
                3usize,
            ),
        ];
        for (name, source, expected) in sources {
            let production = source.split("#[cfg(test)]\nmod ").next().unwrap();
            let mut seen = 0;
            for needle in ["session_lock::check(", "session_lock::is_unlocked("] {
                for (at, _) in production.match_indices(needle) {
                    seen += 1;
                    let call: String = production[at..].chars().take(160).collect();
                    let call = call.split(')').next().unwrap_or_default().to_string();
                    assert!(call.contains("session_id"), "{name}: {call}");
                    assert!(!call.contains("generation"), "{name}: {call}");
                }
            }
            assert_eq!(seen, expected, "{name}: a gate was added or removed");
        }
    }

    /// Two sessions that share a generation are still two connections.
    #[test]
    fn two_sessions_with_one_generation_do_not_share_a_proof() {
        let stored = cheap("hunter2");
        let mut lock = SessionLock::default();
        let now = Instant::now();
        let (one, two) = (
            session_of("4b6c0f6e-3d63-4d0e-9f4f-1d0a2f8e5a11"),
            session_of("9a1e55a4-83f1-4a8e-b0a5-7c4f6a3d21c0"),
        );
        assert!(lock
            .check(Some(&stored), "p", one, Some("hunter2"), now)
            .is_ok());
        assert_eq!(
            lock.check(Some(&stored), "p", two, None, now),
            Err(PASSWORD_REQUIRED.into())
        );
    }

    /// The window between `begin` and `finish` (hashing runs outside the
    /// lock): a right answer to a question asked before an unpair or a
    /// password change unlocks nothing.
    #[test]
    fn an_answer_that_straddles_an_unpair_or_a_password_change_unlocks_nothing() {
        let stored = cheap("hunter2");
        let now = Instant::now();
        for event in ["unpair", "password change"] {
            let mut lock = SessionLock::default();
            let Precheck::Verify(epoch) = lock.begin(Some(&stored), "p", 1, Some("hunter2"), now)
            else {
                panic!("a first offer is verified");
            };
            // ... hashing happens here, outside the lock ...
            if event == "unpair" {
                lock.forget_unlock("p");
            } else {
                lock.relock_all();
            }
            assert_eq!(
                lock.finish("p", 1, "hunter2", true, epoch, now),
                Err(PASSWORD_REQUIRED.into()),
                "{event}"
            );
            assert!(
                lock.check(Some(&stored), "p", 1, None, now).is_err(),
                "{event}"
            );
        }
    }

    fn session_of(attempt: &str) -> u64 {
        crate::device_links::webrtc::WebRtcWireBinding::from_attempt("desk-id", "phone-id", attempt)
            .unwrap()
            .session_id
    }

    /// The real per-connection id, from three attempts of one device: what
    /// one connection proved, the next (a rebuild, a phone restart) must prove
    /// again; an unpair or a new identity forgets it; a password change
    /// relocks everyone.
    #[test]
    fn a_proved_password_never_carries_to_the_next_connection_of_the_same_device() {
        let stored = cheap("hunter2");
        let first = session_of("4b6c0f6e-3d63-4d0e-9f4f-1d0a2f8e5a11");
        let rebuilt = session_of("9a1e55a4-83f1-4a8e-b0a5-7c4f6a3d21c0");
        let after_restart = session_of("0e3b9d44-6a52-4c1b-8d7c-52f1c9a07e63");
        assert!(first != rebuilt && rebuilt != after_restart && first != after_restart);
        let mut lock = SessionLock::default();
        let now = Instant::now();
        assert_eq!(
            lock.check(Some(&stored), "phone-id", first, None, now),
            Err(PASSWORD_REQUIRED.into())
        );
        assert!(lock
            .check(Some(&stored), "phone-id", first, Some("hunter2"), now)
            .is_ok());
        assert!(lock
            .check(Some(&stored), "phone-id", first, None, now)
            .is_ok());
        for next in [rebuilt, after_restart] {
            assert_eq!(
                lock.check(Some(&stored), "phone-id", next, None, now),
                Err(PASSWORD_REQUIRED.into()),
                "a new connection is asked again"
            );
        }
        assert!(lock
            .check(Some(&stored), "phone-id", rebuilt, Some("hunter2"), now)
            .is_ok());
        assert!(lock
            .check(Some(&stored), "phone-id", first, None, now)
            .is_err());
        lock.forget_unlock("phone-id");
        assert_eq!(
            lock.check(Some(&stored), "phone-id", rebuilt, None, now),
            Err(PASSWORD_REQUIRED.into())
        );
        assert!(lock
            .check(Some(&stored), "phone-id", rebuilt, Some("hunter2"), now)
            .is_ok());
        lock.relock_all();
        assert!(lock
            .check(Some(&stored), "phone-id", rebuilt, None, now)
            .is_err());
    }

    #[test]
    fn wrong_guesses_lock_out_with_doubling_and_reconnecting_does_not_help() {
        let stored = cheap("right");
        let mut lock = SessionLock::default();
        let now = Instant::now();
        for guess in ["w1", "w2", "w3", "w4"] {
            assert_eq!(
                lock.check(Some(&stored), "p", 1, Some(guess), now),
                Err(PASSWORD_WRONG.into())
            );
        }
        // The same wrong password again is refused but not counted.
        assert_eq!(
            lock.check(Some(&stored), "p", 1, Some("w4"), now),
            Err(PASSWORD_WRONG.into())
        );
        assert_eq!(
            lock.check(Some(&stored), "p", 1, Some("w5"), now),
            Err(format!("{PASSWORD_LOCKED}:60"))
        );
        // Even the right password, on a new connection, waits out the lockout.
        assert!(lock
            .check(
                Some(&stored),
                "p",
                2,
                Some("right"),
                now + Duration::from_secs(30)
            )
            .unwrap_err()
            .starts_with(PASSWORD_LOCKED));
        let later = now + Duration::from_secs(61);
        assert_eq!(
            lock.check(Some(&stored), "p", 2, Some("w6"), later),
            Err(format!("{PASSWORD_LOCKED}:120"))
        );
        let much_later = later + Duration::from_secs(121);
        assert!(lock
            .check(Some(&stored), "p", 2, Some("right"), much_later)
            .is_ok());
    }

    #[test]
    fn real_hashes_verify_and_differ_per_salt() {
        let a = hash_password("pw").unwrap();
        let b = hash_password("pw").unwrap();
        assert_ne!(a.salt, b.salt);
        assert!(matches("pw", &a));
        assert!(!matches("pW", &a));
    }
}
