//! One anonymous HTTPS GET of the manifest, over `openssl` (certificate and
//! host name are checked; TLS 1.2 or newer), as small as it can be: HTTP/1.0,
//! no keep-alive, no compression, no chunking, no proxy.
//!
//! Limits: 10 s in all (connect, request and body), a body cap of 64 KiB (the
//! read stops there), at most 3 redirects, each to the same host. Plain
//! `http` is accepted only for a loopback address in a debug build (the local
//! test server). The request carries exactly `Host`, `User-Agent:
//! DeskLink-update-check/<version>`, `Accept` and, when one is stored,
//! `If-None-Match`: nothing about the person, the computer or any phone.
//!
//! The body is only bytes here: whether it can be trusted is [`super::model::verify`]'s job.
//!
//! Sibling edges: `url`, `model` (the size cap), `store` (the ETag rule).

use super::model::MAX_MANIFEST_BYTES;
use super::store::etag_ok;
use super::url::Url;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

const TOTAL: Duration = Duration::from_secs(10);
const MAX_REDIRECTS: usize = 3;
const MAX_HEADER_BYTES: usize = 16 * 1024;

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Fetched {
    NotModified,
    Body {
        bytes: Vec<u8>,
        etag: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FetchError {
    Offline,
    Timeout,
    Http,
    Oversized,
    /// Not a response we accept: bad status line, a redirect we refuse, a
    /// scheme we refuse, a truncated body.
    Parse,
}

impl FetchError {
    pub(super) fn code(self) -> &'static str {
        match self {
            FetchError::Offline => "offline",
            FetchError::Timeout => "timeout",
            FetchError::Http => "http",
            FetchError::Oversized => "oversized",
            FetchError::Parse => "parse",
        }
    }
}

/// The seam between the engine and the network: the real one below, a fake in
/// the tests.
pub(super) trait Fetch: Send {
    fn get(&self, etag: Option<&str>) -> Result<Fetched, FetchError>;
}

pub(super) struct HttpsFetcher {
    url: Url,
    /// The budget for the whole exchange (10 s; shorter in tests).
    total: Duration,
    /// A certificate the tests trust besides the system's.
    #[cfg(test)]
    pub(super) extra_root: Option<openssl::x509::X509>,
}

/// Whether `url` may be fetched at all in this build.
pub(super) fn url_allowed(url: &Url, debug_build: bool) -> bool {
    if url.https {
        url.port.is_none() || url.port == Some(443) || debug_build
    } else {
        debug_build && url.is_loopback()
    }
}

impl HttpsFetcher {
    /// `None` when the address is not one this build may fetch.
    pub(super) fn new(url: &str) -> Option<HttpsFetcher> {
        let url = Url::parse(url).filter(|url| url_allowed(url, cfg!(debug_assertions)))?;
        Some(HttpsFetcher {
            url,
            total: TOTAL,
            #[cfg(test)]
            extra_root: None,
        })
    }
}

#[cfg(test)]
impl HttpsFetcher {
    pub(super) fn set_total(&mut self, total: Duration) {
        self.total = total;
    }
}

enum Step {
    Done(Fetched),
    Redirect(String),
}

fn remaining(deadline: Instant) -> Result<Duration, FetchError> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|left| !left.is_zero())
        .ok_or(FetchError::Timeout)
}

fn io_error(error: &std::io::Error) -> FetchError {
    match error.kind() {
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => FetchError::Timeout,
        _ => FetchError::Offline,
    }
}

impl Fetch for HttpsFetcher {
    fn get(&self, etag: Option<&str>) -> Result<Fetched, FetchError> {
        let deadline = Instant::now() + self.total;
        let etag = etag.filter(|e| etag_ok(e));
        let mut url = self.url.clone();
        for _ in 0..=MAX_REDIRECTS {
            match self.once(&url, etag, deadline)? {
                Step::Done(done) => return Ok(done),
                Step::Redirect(location) => {
                    let next = resolve(&url, &location).ok_or(FetchError::Parse)?;
                    if next.host != self.url.host
                        || next.effective_port() != self.url.effective_port()
                        || next.https != url.https
                        || !url_allowed(&next, cfg!(debug_assertions))
                    {
                        return Err(FetchError::Parse);
                    }
                    url = next;
                }
            }
        }
        Err(FetchError::Parse)
    }
}

/// The first address of `host`, looked up on a helper thread so a slow
/// resolver cannot hold the exchange past its deadline (the thread is left to
/// finish on its own).
fn resolve_within(
    host: &str,
    port: u16,
    deadline: Instant,
) -> Result<std::net::SocketAddr, FetchError> {
    let (sender, receiver) = std::sync::mpsc::channel();
    let host = host.to_string();
    std::thread::Builder::new()
        .name("desklink-update-dns".into())
        .spawn(move || {
            let _ = sender.send(
                (host.as_str(), port)
                    .to_socket_addrs()
                    .ok()
                    .and_then(|mut addrs| addrs.next()),
            );
        })
        .map_err(|_| FetchError::Offline)?;
    match receiver.recv_timeout(remaining(deadline)?) {
        Ok(Some(addr)) => Ok(addr),
        Ok(None) => Err(FetchError::Offline),
        Err(_) => Err(FetchError::Timeout),
    }
}

/// A `Location` value against the URL it answered: an absolute URL or an
/// absolute path; nothing else.
fn resolve(base: &Url, location: &str) -> Option<Url> {
    if location.starts_with('/') && !location.starts_with("//") {
        let mut next = base.clone();
        next.target = Url::parse(&format!("http://h{location}"))?.target;
        return Some(next);
    }
    Url::parse(location)
}

impl HttpsFetcher {
    fn once(&self, url: &Url, etag: Option<&str>, deadline: Instant) -> Result<Step, FetchError> {
        let addr = resolve_within(&url.host, url.effective_port(), deadline)?;
        let tcp = TcpStream::connect_timeout(&addr, remaining(deadline)?)
            .map_err(|error| io_error(&error))?;
        let mut request = format!(
            "GET {} HTTP/1.0\r\nHost: {}\r\nUser-Agent: DeskLink-update-check/{}\r\nAccept: application/json\r\n",
            url.target,
            match url.port {
                Some(port) => format!("{}:{port}", url.host),
                None => url.host.clone(),
            },
            crate::config::VERSION,
        );
        if let Some(etag) = etag {
            request.push_str(&format!("If-None-Match: {etag}\r\n"));
        }
        request.push_str("\r\n");
        if url.https {
            let mut builder =
                openssl::ssl::SslConnector::builder(openssl::ssl::SslMethod::tls_client())
                    .map_err(|_| FetchError::Offline)?;
            builder
                .set_min_proto_version(Some(openssl::ssl::SslVersion::TLS1_2))
                .map_err(|_| FetchError::Offline)?;
            #[cfg(test)]
            if let Some(root) = &self.extra_root {
                let mut store = openssl::x509::store::X509StoreBuilder::new()
                    .map_err(|_| FetchError::Offline)?;
                store
                    .add_cert(root.clone())
                    .map_err(|_| FetchError::Offline)?;
                builder.set_cert_store(store.build());
            }
            let connector = builder.build();
            let stream = Bounded::new(tcp, deadline)?;
            let tls = connector
                .connect(&url.host, stream)
                .map_err(|_| FetchError::Offline)?;
            exchange(tls, &request, deadline)
        } else {
            exchange(Bounded::new(tcp, deadline)?, &request, deadline)
        }
    }
}

/// A socket whose every read and write is cut off at the shared deadline.
struct Bounded {
    tcp: TcpStream,
    deadline: Instant,
}

impl Bounded {
    fn new(tcp: TcpStream, deadline: Instant) -> Result<Bounded, FetchError> {
        let _ = tcp.set_nodelay(true);
        Ok(Bounded { tcp, deadline })
    }

    fn arm(&self) -> std::io::Result<()> {
        let left = remaining(self.deadline)
            .map_err(|_| std::io::Error::from(std::io::ErrorKind::TimedOut))?;
        self.tcp.set_read_timeout(Some(left))?;
        self.tcp.set_write_timeout(Some(left))
    }
}

impl Read for Bounded {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.arm()?;
        self.tcp.read(buf)
    }
}

impl Write for Bounded {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.arm()?;
        self.tcp.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.tcp.flush()
    }
}

fn exchange<S: Read + Write>(
    mut stream: S,
    request: &str,
    deadline: Instant,
) -> Result<Step, FetchError> {
    stream
        .write_all(request.as_bytes())
        .and_then(|()| stream.flush())
        .map_err(|error| io_error(&error))?;
    // Read until the head is complete, then the body up to the cap (+1 byte
    // to tell "exactly the cap" from "over it").
    let mut data: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(at) = find(&data, b"\r\n\r\n") {
            break at;
        }
        if data.len() > MAX_HEADER_BYTES {
            return Err(FetchError::Parse);
        }
        remaining(deadline)?;
        match stream.read(&mut chunk) {
            Ok(0) => return Err(FetchError::Parse),
            Ok(n) => data.extend_from_slice(&chunk[..n]),
            Err(error) => return Err(io_error(&error)),
        }
    };
    let head = std::str::from_utf8(&data[..head_end])
        .map_err(|_| FetchError::Parse)?
        .to_string();
    let mut lines = head.split("\r\n");
    let status: u16 = {
        let line = lines.next().ok_or(FetchError::Parse)?;
        let mut parts = line.split(' ');
        if !parts
            .next()
            .is_some_and(|v| v == "HTTP/1.0" || v == "HTTP/1.1")
        {
            return Err(FetchError::Parse);
        }
        parts
            .next()
            .and_then(|code| code.parse().ok())
            .ok_or(FetchError::Parse)?
    };
    let (mut etag, mut location, mut length) = (None, None, None);
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match name.trim().to_ascii_lowercase().as_str() {
            "etag" => etag = Some(value.to_string()),
            "location" => location = Some(value.to_string()),
            "content-length" => length = value.parse::<usize>().ok(),
            // Nothing is asked for that needs decoding: refuse what arrives.
            "transfer-encoding" | "content-encoding" => return Err(FetchError::Parse),
            _ => {}
        }
    }
    match status {
        304 => return Ok(Step::Done(Fetched::NotModified)),
        301 | 302 | 303 | 307 | 308 => {
            return location.map(Step::Redirect).ok_or(FetchError::Parse)
        }
        200 => {}
        _ => return Err(FetchError::Http),
    }
    if length.is_some_and(|len| len > MAX_MANIFEST_BYTES) {
        return Err(FetchError::Oversized);
    }
    let mut body = data[head_end + 4..].to_vec();
    loop {
        if body.len() > MAX_MANIFEST_BYTES {
            return Err(FetchError::Oversized);
        }
        if length.is_some_and(|len| body.len() >= len) {
            break;
        }
        remaining(deadline)?;
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => body.extend_from_slice(&chunk[..n]),
            // A TLS peer that closes without close_notify after a complete body.
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(error) => return Err(io_error(&error)),
        }
    }
    if length.is_some_and(|len| body.len() != len) {
        return Err(FetchError::Parse);
    }
    let etag = etag.filter(|e| etag_ok(e));
    Ok(Step::Done(Fetched::Body { bytes: body, etag }))
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}
