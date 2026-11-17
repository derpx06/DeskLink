//! Tests of the update module, by topic. The shared fixtures
//! (`testdata/update/`, byte-identical copies of the workspace ones) drive the
//! verification and decision tables; everything else uses a fake clock, a
//! fake fetcher or a local test server.

use super::engine::Settings;
use super::fetch::{Fetch, FetchError, Fetched};
use super::model::{Key, Version};
use base64::Engine as _;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

mod engine;
mod fetch;
mod misc;
mod model;
mod open;
mod scan;
mod sim;
mod strict;

pub(super) const NOW: u64 = 1_795_003_600;
pub(super) const HOST: &str = "desklink-updates.web.app";

pub(super) fn fixture_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/update")
}

pub(super) fn fixture(path: &str) -> Vec<u8> {
    std::fs::read(fixture_dir().join(path)).unwrap_or_else(|e| panic!("{path}: {e}"))
}

pub(super) fn json(path: &str) -> serde_json::Value {
    serde_json::from_slice(&fixture(path)).unwrap()
}

/// The shared test keys (`k1`, `k2`).
pub(super) fn keys() -> Vec<Key> {
    json("keys.json")["keys"]
        .as_array()
        .unwrap()
        .iter()
        .map(|k| Key {
            kid: k["kid"].as_str().unwrap().to_string(),
            public: base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(k["public"].as_str().unwrap())
                .unwrap()
                .try_into()
                .unwrap(),
        })
        .collect()
}

pub(super) fn v(text: &str) -> Version {
    Version::parse(text).unwrap()
}

pub(super) fn settings() -> Settings {
    Settings {
        auto_check: true,
        channel: String::new(),
    }
}

/// A scripted fetcher: answers in order, counts calls, remembers the ETags it
/// was asked with.
#[derive(Clone, Default)]
pub(super) struct Fake {
    pub(super) script: Arc<Mutex<VecDeque<Result<Fetched, FetchError>>>>,
    pub(super) calls: Arc<AtomicUsize>,
    pub(super) etags: Arc<Mutex<Vec<Option<String>>>>,
}

impl Fake {
    pub(super) fn push(&self, answer: Result<Fetched, FetchError>) {
        self.script.lock().unwrap().push_back(answer);
    }

    pub(super) fn body(&self, bytes: Vec<u8>, etag: &str) {
        self.push(Ok(Fetched::Body {
            bytes,
            etag: Some(etag.to_string()),
        }));
    }

    pub(super) fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl Fetch for Fake {
    fn get(&self, etag: Option<&str>) -> Result<Fetched, FetchError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.etags.lock().unwrap().push(etag.map(str::to_string));
        self.script
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Err(FetchError::Offline))
    }
}

/// A fresh scratch file path for a state file.
pub(super) fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "desklink-update-test-{}-{name}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("update.json")
}

/// The first shared test key: its id and private key.
fn signing_key() -> (String, openssl::pkey::PKey<openssl::pkey::Private>) {
    let kid = json("keys.json")["keys"][0]["kid"]
        .as_str()
        .unwrap()
        .to_string();
    let mut pems: Vec<_> = std::fs::read_dir(fixture_dir())
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.to_string_lossy().ends_with(".private.pem"))
        .collect();
    pems.sort();
    let pem = std::fs::read(&pems[0]).unwrap();
    (
        kid,
        openssl::pkey::PKey::private_key_from_pem(&pem).unwrap(),
    )
}

/// An envelope around `payload`, signed with the first shared test key.
pub(super) fn sign(payload: &str) -> Vec<u8> {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD as B;
    let (kid, key) = signing_key();
    let mut signer = openssl::sign::Signer::new_without_digest(&key).unwrap();
    let sig = signer.sign_oneshot_to_vec(payload.as_bytes()).unwrap();
    serde_json::json!({"v":1,"kid":kid,"payload":B.encode(payload),"sig":B.encode(sig)})
        .to_string()
        .into_bytes()
}

/// A raw envelope with any parts, for the cases the helper above refuses to build.
pub(super) fn envelope(v: &str, kid: &str, payload: &str, sig: &str) -> Vec<u8> {
    format!(r#"{{"v":{v},"kid":"{kid}","payload":"{payload}","sig":"{sig}"}}"#).into_bytes()
}
