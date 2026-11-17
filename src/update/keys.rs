//! The public keys manifests are checked against: `k1` (current) and `k2`
//! (next, for rotation), compiled in. Debug builds also carry the `test1`/`test2` keys
//! so the local test server's manifests verify; release builds do not contain
//! it (a test pins the key set; the gate's last check is that the release
//! binary holds none of the test keys' text: `strings` on it finds neither
//! the base64 below nor `test1`). One source of truth: `docs/update/keys.json`.
//!
//! Sibling edges: `model` (the `Key` type). Outside: `base64`.

use super::model::Key;
use base64::Engine as _;

/// (kid, 32-byte public key as base64url). An empty key is a slot the owner
/// has not filled yet (`docs/UPDATES.md` 3.8 step 5): it trusts nothing, so
/// every manifest is refused until it is set.
const PRODUCTION: [(&str, &str); 2] = [("k1", ""), ("k2", "")];

/// The throwaway test keys (`test1`, `test2`; their private halves are in the
/// shared fixtures, openly labelled). Debug builds only: a release build does
/// not contain them (`release_keys` is the whole release set, and a test pins
/// that none of the fixtures' keys is in it).
#[cfg(debug_assertions)]
const TEST: [(&str, &str); 2] = [
    ("test1", "1ESE_TECkkgupykava9EhlyV7zD9PZRlo22v2jNoKS0"),
    ("test2", "gdqQHUmHGByYeiKgAEQ7735GcnSB-3C0oQopuPHKZ-g"),
];

fn decode(kid: &str, text: &str) -> Option<Key> {
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(text)
        .ok()?;
    Some(Key {
        kid: kid.to_string(),
        public: raw.try_into().ok()?,
    })
}

/// The keys of a release build: the project's own, nothing else.
pub(super) fn release_keys() -> Vec<Key> {
    PRODUCTION
        .iter()
        .filter_map(|(kid, key)| decode(kid, key))
        .collect()
}

/// Every key this build trusts: the release set, plus the test keys in a
/// debug build.
pub(super) fn trusted() -> Vec<Key> {
    #[cfg(debug_assertions)]
    let mut keys = release_keys();
    #[cfg(not(debug_assertions))]
    let keys = release_keys();
    #[cfg(debug_assertions)]
    keys.extend(TEST.iter().filter_map(|(kid, key)| decode(kid, key)));
    keys
}
