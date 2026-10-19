//! Short-lived TURN credentials for a mutual pair.
//!
//! The relay never runs media; it only mints credentials, so a TURN
//! allocation can be tied to an authenticated pair and a daily quota.

use std::time::Duration;

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use openssl::hash::MessageDigest;
use openssl::pkey::PKey;
use openssl::sign::Signer;
use serde::Deserialize;

use crate::config::{Config, TurnBackendKind};
use crate::protocol::IceServer;

/// Lifetime requested from Cloudflare. Clients request fresh credentials on
/// every rebuild, so one hour costs nothing and bounds a leaked credential.
const CLOUDFLARE_TTL: u64 = 3_600;
const CLOUDFLARE_TIMEOUT: Duration = Duration::from_secs(5);

pub enum TurnBackend {
    None,
    Cloudflare {
        agent: ureq::Agent,
        url: String,
        api_token: String,
    },
    Coturn {
        static_secret: String,
        urls: Vec<String>,
        ttl: u64,
    },
}

impl TurnBackend {
    pub fn from_config(config: &Config) -> Self {
        match config.turn_backend {
            TurnBackendKind::None => TurnBackend::None,
            TurnBackendKind::Cloudflare => TurnBackend::Cloudflare {
                agent: ureq::AgentBuilder::new()
                    .timeout(CLOUDFLARE_TIMEOUT)
                    .build(),
                url: format!(
                    "https://rtc.live.cloudflare.com/v1/turn/keys/{}/credentials/generate-ice-servers",
                    config.cloudflare.key_id
                ),
                api_token: config.cloudflare.api_token.clone(),
            },
            TurnBackendKind::Coturn => TurnBackend::Coturn {
                static_secret: config.coturn.static_secret.clone(),
                urls: config.coturn.urls.clone(),
                ttl: config.coturn.ttl,
            },
        }
    }

    pub fn is_enabled(&self) -> bool {
        !matches!(self, TurnBackend::None)
    }

    /// Returns the ICE servers and their lifetime in seconds. Errors are
    /// generic on purpose; the caller only reports `turn-denied`.
    pub fn credentials(&self, fp: &str, now_unix: u64) -> Result<(Vec<IceServer>, u64), String> {
        match self {
            TurnBackend::None => Err("no TURN backend configured".into()),
            TurnBackend::Cloudflare {
                agent,
                url,
                api_token,
            } => {
                let response: CloudflareResponse = agent
                    .post(url)
                    .set("Authorization", &format!("Bearer {api_token}"))
                    .send_json(serde_json::json!({ "ttl": CLOUDFLARE_TTL }))
                    .map_err(|error| format!("Cloudflare TURN request failed: {}", error.kind()))?
                    .into_json()
                    .map_err(|_| "Cloudflare TURN response was not JSON".to_string())?;
                Ok((response.ice_servers.into_ice_servers(), CLOUDFLARE_TTL))
            }
            TurnBackend::Coturn {
                static_secret,
                urls,
                ttl,
            } => {
                let username = coturn_username(now_unix + ttl, fp);
                let credential = coturn_credential(static_secret, &username)?;
                Ok((
                    vec![IceServer {
                        urls: urls.clone(),
                        username: Some(username),
                        credential: Some(credential),
                    }],
                    *ttl,
                ))
            }
        }
    }
}

/// coturn `use-auth-secret` username: `<unix expiry>:<label>`. The label is a
/// fingerprint prefix so coturn's own logs can tell sessions apart without
/// carrying the whole address.
pub fn coturn_username(expiry_unix: u64, fp: &str) -> String {
    let label: String = fp.chars().take(16).collect();
    format!("{expiry_unix}:{label}")
}

/// coturn REST credential: base64(HMAC-SHA1(static secret, username)).
pub fn coturn_credential(secret: &str, username: &str) -> Result<String, String> {
    let key = PKey::hmac(secret.as_bytes()).map_err(|error| error.to_string())?;
    let mut signer = Signer::new(MessageDigest::sha1(), &key).map_err(|error| error.to_string())?;
    signer
        .update(username.as_bytes())
        .map_err(|error| error.to_string())?;
    let mac = signer.sign_to_vec().map_err(|error| error.to_string())?;
    Ok(STANDARD.encode(mac))
}

/// Cloudflare has returned `iceServers` both as one object and as an array,
/// and `urls` both as a string and as an array; accept every combination.
#[derive(Deserialize)]
#[serde(untagged)]
enum OneOrMany<T> {
    One(T),
    Many(Vec<T>),
}

impl<T> OneOrMany<T> {
    fn into_vec(self) -> Vec<T> {
        match self {
            OneOrMany::One(item) => vec![item],
            OneOrMany::Many(items) => items,
        }
    }
}

#[derive(Deserialize)]
struct CloudflareResponse {
    #[serde(rename = "iceServers")]
    ice_servers: OneOrMany<RawIceServer>,
}

#[derive(Deserialize)]
struct RawIceServer {
    urls: OneOrMany<String>,
    username: Option<String>,
    credential: Option<String>,
}

impl OneOrMany<RawIceServer> {
    fn into_ice_servers(self) -> Vec<IceServer> {
        self.into_vec()
            .into_iter()
            .map(|raw| IceServer {
                urls: raw.urls.into_vec(),
                username: raw.username,
                credential: raw.credential,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coturn_credential_matches_the_rfc_2202_hmac_sha1_vector() {
        // RFC 2202 test case 2: HMAC-SHA1("Jefe", "what do ya want for nothing?")
        // = effcdf6ae5eb2fa2d27416d5f184df9c259a7c79.
        assert_eq!(
            coturn_credential("Jefe", "what do ya want for nothing?").unwrap(),
            "7/zfauXrL6LSdBbV8YTfnCWafHk="
        );
    }

    #[test]
    fn coturn_credentials_carry_expiry_and_fingerprint_prefix() {
        let backend = TurnBackend::Coturn {
            static_secret: "s3cret".into(),
            urls: vec!["turn:t.example:3478?transport=udp".into()],
            ttl: 3600,
        };
        let (servers, ttl) = backend
            .credentials("abcdefghijklmnopqrstuvwxyz0123456789ABCDEFG", 1_700_000_000)
            .unwrap();
        assert_eq!(ttl, 3600);
        let username = servers[0].username.clone().unwrap();
        assert_eq!(username, "1700003600:abcdefghijklmnop");
        assert_eq!(
            servers[0].credential.clone().unwrap(),
            coturn_credential("s3cret", &username).unwrap()
        );
    }

    #[test]
    fn cloudflare_object_and_array_shapes_normalize_to_arrays() {
        let object: CloudflareResponse = serde_json::from_str(
            r#"{"iceServers":{"urls":["turn:a:3478"],"username":"u","credential":"c"}}"#,
        )
        .unwrap();
        let array: CloudflareResponse = serde_json::from_str(
            r#"{"iceServers":[{"urls":"stun:a:3478"},{"urls":["turn:a:3478","turns:a:5349"],"username":"u","credential":"c"}]}"#,
        )
        .unwrap();
        assert_eq!(
            object.ice_servers.into_ice_servers(),
            vec![IceServer {
                urls: vec!["turn:a:3478".into()],
                username: Some("u".into()),
                credential: Some("c".into()),
            }]
        );
        let array = array.ice_servers.into_ice_servers();
        assert_eq!(array.len(), 2);
        assert_eq!(array[0].urls, vec!["stun:a:3478".to_string()]);
        assert_eq!(array[0].username, None);
        assert_eq!(array[1].urls.len(), 2);
    }

    #[test]
    fn none_backend_is_disabled() {
        assert!(!TurnBackend::None.is_enabled());
        assert!(TurnBackend::None.credentials("fp", 0).is_err());
    }
}
