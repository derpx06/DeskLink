//! A minimal absolute-URL parser for the two places the update code takes a
//! URL from outside: the update address and the links in a manifest. It
//! accepts only what those need (`http`/`https`, a plain host, an optional
//! port, a path) and rejects everything odd, instead of guessing.
//!
//! Sibling edges: none. Outside: std only.

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Url {
    pub(super) https: bool,
    /// Lower-case ASCII host (letters, digits, dots, hyphens).
    pub(super) host: String,
    /// `None` is the scheme's default (443 or 80).
    pub(super) port: Option<u16>,
    /// Path and query, always starting with `/`.
    pub(super) target: String,
}

impl Url {
    /// `None` for anything that is not a plain `http(s)://host[:port]/path`:
    /// user information, an IPv6 literal, a fragment, spaces or control
    /// characters, a non-ASCII byte, an empty host.
    pub(super) fn parse(text: &str) -> Option<Url> {
        if text.len() > 2048 || text.bytes().any(|b| b <= 0x20 || b >= 0x7f || b == b'\\') {
            return None;
        }
        let (https, rest) = if let Some(rest) = text.strip_prefix("https://") {
            (true, rest)
        } else {
            (false, text.strip_prefix("http://")?)
        };
        if rest.contains('#') {
            return None;
        }
        let end = rest.find(['/', '?']).unwrap_or(rest.len());
        let (authority, tail) = rest.split_at(end);
        if authority.contains('@') {
            return None;
        }
        let (host, port) = match authority.split_once(':') {
            Some((host, port)) => {
                if port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
                    return None;
                }
                (host, Some(port.parse::<u16>().ok().filter(|p| *p != 0)?))
            }
            None => (authority, None),
        };
        let host = host.to_ascii_lowercase();
        if host.is_empty()
            || host.starts_with(['.', '-'])
            || host.ends_with(['-'])
            || host.contains("..")
            || !host
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'-')
        {
            return None;
        }
        let target = if tail.is_empty() {
            "/".to_string()
        } else if tail.starts_with('?') {
            format!("/{tail}")
        } else {
            tail.to_string()
        };
        Some(Url {
            https,
            host,
            port,
            target,
        })
    }

    pub(super) fn effective_port(&self) -> u16 {
        self.port.unwrap_or(if self.https { 443 } else { 80 })
    }

    /// Loopback names: the only place plain `http` is ever allowed (debug
    /// builds, the local test server).
    pub(super) fn is_loopback(&self) -> bool {
        self.host == "localhost" || self.host == "127.0.0.1"
    }

    pub(super) fn text(&self) -> String {
        let scheme = if self.https { "https" } else { "http" };
        match self.port {
            Some(port) => format!("{scheme}://{}:{port}{}", self.host, self.target),
            None => format!("{scheme}://{}{}", self.host, self.target),
        }
    }
}
