//! TOML configuration. Every field except `public_host` has a default, and
//! secrets may come from the environment so they never sit in a mounted file.

use std::net::IpAddr;
use std::path::PathBuf;

use serde::Deserialize;

pub const ENV_CF_TOKEN: &str = "DESKLINK_RELAY_CF_TOKEN";
pub const ENV_TURN_SECRET: &str = "DESKLINK_RELAY_TURN_SECRET";

// No `Debug` on the config structs: they hold API tokens and an accidental
// `{:?}` in a log line would leak them.
#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Plain WebSocket listener. TLS is terminated by the reverse proxy.
    pub listen: String,
    /// Host name clients connect to; part of the signed auth string.
    pub public_host: String,
    /// Peers whose `X-Forwarded-For` is believed. Anyone else could forge it.
    pub trusted_proxies: Vec<IpAddr>,
    pub turn_backend: TurnBackendKind,
    pub cloudflare: CloudflareConfig,
    pub coturn: CoturnConfig,
    /// TURN quota counters survive restarts here. `None` keeps them in memory.
    pub state_file: Option<PathBuf>,
    pub limits: Limits,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:8080".into(),
            public_host: String::new(),
            trusted_proxies: vec![
                IpAddr::from([127, 0, 0, 1]),
                IpAddr::from([0, 0, 0, 0, 0, 0, 0, 1]),
            ],
            turn_backend: TurnBackendKind::None,
            cloudflare: CloudflareConfig::default(),
            coturn: CoturnConfig::default(),
            state_file: None,
            limits: Limits::default(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TurnBackendKind {
    None,
    Cloudflare,
    Coturn,
}

#[derive(Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CloudflareConfig {
    pub key_id: String,
    pub api_token: String,
}

#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CoturnConfig {
    pub static_secret: String,
    pub urls: Vec<String>,
    pub ttl: u64,
}

impl Default for CoturnConfig {
    fn default() -> Self {
        Self {
            static_secret: String::new(),
            urls: Vec::new(),
            // Clients ask again on every rebuild, so an hour costs nothing
            // and limits how long a leaked credential is useful.
            ttl: 3_600,
        }
    }
}

/// Abuse limits. IPv4 numbers are generous because mobile carriers (notably
/// in India) put thousands of subscribers behind one CGNAT address; an IPv6
/// /64 is one subscriber, so it gets the strict numbers.
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    pub msgs_per_sec: f64,
    pub msgs_burst: f64,
    pub bytes_per_sec: f64,
    pub max_frame_bytes: usize,
    pub connections_per_ipv4: usize,
    pub connections_per_ipv6_64: usize,
    pub auths_per_min_ipv4: f64,
    pub auths_per_min_ipv6_64: f64,
    /// Totals for a whole IPv6 /48, on top of the /64 numbers, so a routed
    /// prefix cannot multiply its limits by rotating /64s.
    pub connections_per_ipv6_48: usize,
    pub auths_per_min_ipv6_48: f64,
    pub global_connections: usize,
    /// Fraction of `global_connections` that sockets which have not yet
    /// authenticated may hold; the rest is reserved for authenticated ones.
    pub unauthenticated_share: f64,
    pub turn_per_pair_per_day: u32,
    /// Per IPv4 address or IPv6 /64.
    pub turn_per_network_per_day: u32,
    pub turn_per_ipv6_48_per_day: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            msgs_per_sec: 20.0,
            msgs_burst: 100.0,
            bytes_per_sec: 256.0 * 1024.0,
            max_frame_bytes: 512 * 1024,
            connections_per_ipv4: 64,
            connections_per_ipv6_64: 8,
            auths_per_min_ipv4: 300.0,
            auths_per_min_ipv6_64: 30.0,
            connections_per_ipv6_48: 32,
            auths_per_min_ipv6_48: 120.0,
            global_connections: 10_000,
            unauthenticated_share: 0.5,
            turn_per_pair_per_day: 20,
            turn_per_network_per_day: 48,
            turn_per_ipv6_48_per_day: 192,
        }
    }
}

impl Config {
    /// Parses, applies environment overrides from `env`, and validates.
    pub fn parse(text: &str, env: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let mut config: Config =
            toml::from_str(text).map_err(|error| format!("invalid config: {error}"))?;
        if let Some(token) = env(ENV_CF_TOKEN).filter(|value| !value.is_empty()) {
            config.cloudflare.api_token = token;
        }
        if let Some(secret) = env(ENV_TURN_SECRET).filter(|value| !value.is_empty()) {
            config.coturn.static_secret = secret;
        }
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), String> {
        if self.public_host.trim().is_empty() {
            return Err("public_host is required (the relay's public DNS name)".into());
        }
        match self.turn_backend {
            TurnBackendKind::Cloudflare
                if self.cloudflare.key_id.is_empty() || self.cloudflare.api_token.is_empty() =>
            {
                Err(format!(
                    "turn_backend = \"cloudflare\" needs cloudflare.key_id and cloudflare.api_token (or {ENV_CF_TOKEN})"
                ))
            }
            TurnBackendKind::Coturn
                if self.coturn.static_secret.is_empty() || self.coturn.urls.is_empty() =>
            {
                Err(format!(
                    "turn_backend = \"coturn\" needs coturn.urls and coturn.static_secret (or {ENV_TURN_SECRET})"
                ))
            }
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn minimal_config_takes_defaults() {
        let config = Config::parse(r#"public_host = "relay.example.com""#, no_env).unwrap();
        assert_eq!(config.listen, "127.0.0.1:8080");
        assert_eq!(config.trusted_proxies.len(), 2);
        assert_eq!(config.turn_backend, TurnBackendKind::None);
        assert_eq!(config.limits.connections_per_ipv4, 64);
        assert_eq!(config.limits.max_frame_bytes, 512 * 1024);
    }

    #[test]
    fn public_host_is_required_and_typos_are_rejected() {
        assert!(Config::parse("", no_env).is_err());
        assert!(Config::parse("public_host = \"h\"\nlisen = \"x\"", no_env).is_err());
    }

    #[test]
    fn secrets_come_from_the_environment() {
        let text = r#"
            public_host = "h"
            turn_backend = "cloudflare"
            [cloudflare]
            key_id = "k"
        "#;
        assert!(Config::parse(text, no_env).is_err());
        let config = Config::parse(text, |name| {
            (name == ENV_CF_TOKEN).then(|| "from-env".to_string())
        })
        .unwrap();
        assert_eq!(config.cloudflare.api_token, "from-env");
    }

    #[test]
    fn coturn_needs_urls_and_secret() {
        let text = r#"
            public_host = "h"
            turn_backend = "coturn"
            [coturn]
            urls = ["turn:h:3478?transport=udp"]
        "#;
        assert!(Config::parse(text, no_env).is_err());
        let config = Config::parse(text, |name| {
            (name == ENV_TURN_SECRET).then(|| "s".to_string())
        })
        .unwrap();
        assert_eq!(config.coturn.ttl, 3_600);
    }
}
