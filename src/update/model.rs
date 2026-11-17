//! The pure core: versions, the signed manifest envelope and its checks,
//! channel selection, the staged-rollout bucket and the decision
//! (`docs/UPDATES.md` sections 2 and 4.3). No I/O, no clock of its own, no GTK.
//!
//! Nothing in a manifest is looked at before its signature has passed:
//! [`verify`] checks the size, the envelope, the key, then the signature, and
//! only then reads the payload.
//!
//! Sibling edges: `keys` (the trusted keys are passed in), `url` (the link
//! allowlist). Outside: `openssl`, `serde_json`, `base64`.

use super::strict_json;
use base64::Engine as _;

/// A manifest is at most this many bytes (the fetch stops reading there too).
pub(super) const MAX_MANIFEST_BYTES: usize = 64 * 1024;
/// How far ahead of this clock a manifest may have been issued (an hour).
const FUTURE_SLACK_SECS: u64 = 3_600;
/// The longest a manifest may be valid for (the publisher uses 21 days).
const MAX_VALIDITY_SECS: u64 = 22 * 86_400;
const MAX_NOTES: usize = 8;
const MAX_TITLE: usize = 40;
const MAX_DETAIL: usize = 80;
const MAX_REASON: usize = 160;
/// `MAJOR.MINOR.PATCH`, nothing else (no pre-release tags are used).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct Version(pub(crate) u32, pub(crate) u32, pub(crate) u32);

impl Version {
    pub(crate) fn parse(text: &str) -> Option<Version> {
        let mut parts = text.split('.');
        let mut next = || {
            let part = parts.next()?;
            let digits_ok = !part.is_empty()
                && part.len() <= 10
                && part.bytes().all(|b| b.is_ascii_digit())
                && (part == "0" || !part.starts_with('0'));
            // Each part is at most 2147483647.
            digits_ok
                .then(|| part.parse::<u32>().ok().filter(|n| *n <= 2_147_483_647))
                .flatten()
        };
        let version = Version(next()?, next()?, next()?);
        parts.next().is_none().then_some(version)
    }

    /// Below 1.0.0 is the beta line.
    pub(crate) fn is_beta_line(&self) -> bool {
        self.0 == 0
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Channel {
    Beta,
    Stable,
}

impl Channel {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Channel::Beta => "beta",
            Channel::Stable => "stable",
        }
    }

    /// The stored choice, else the line's default: beta for 0.x, stable from 1.0.
    pub(crate) fn chosen(stored: &str, installed: Version) -> Channel {
        match stored {
            "beta" => Channel::Beta,
            "stable" => Channel::Stable,
            _ if installed.is_beta_line() => Channel::Beta,
            _ => Channel::Stable,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Glyph {
    Refresh,
    Swap,
    Check,
    Files,
    Terminal,
    Speed,
    Shield,
    Star,
}

impl Glyph {
    fn parse(text: &str) -> Glyph {
        match text {
            "refresh" => Glyph::Refresh,
            "swap" => Glyph::Swap,
            "check" => Glyph::Check,
            "files" => Glyph::Files,
            "terminal" => Glyph::Terminal,
            "speed" => Glyph::Speed,
            "shield" => Glyph::Shield,
            _ => Glyph::Star,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Note {
    pub(crate) title: String,
    pub(crate) detail: Option<String>,
    pub(crate) glyph: Glyph,
}

/// The desktop tarball: this release's pinned link and, when the manifest
/// gives a well-formed one, its SHA-256 (lower-case hex).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Download {
    pub(crate) url: String,
    pub(crate) sha256: Option<String>,
}

/// The desktop release a manifest offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Component {
    pub(crate) version: Version,
    pub(crate) released: Option<String>,
    pub(crate) min_supported: Version,
    pub(crate) required_reason: Option<String>,
    /// 0..=100.
    pub(crate) rollout: u8,
    pub(crate) notes: Vec<Note>,
    pub(crate) notes_url: Option<String>,
    pub(crate) release_url: Option<String>,
    pub(crate) tarball: Option<Download>,
}

/// A verified, in-date manifest: only the desktop entry of each channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Payload {
    pub(crate) issued_at: u64,
    pub(crate) expires: u64,
    pub(crate) beta: Option<Component>,
    pub(crate) stable: Option<Component>,
}

/// Why a manifest was refused. Codes only: no text from the server is kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reject {
    Oversized,
    Parse,
    Signature,
    Expired,
    Replay,
    Schema,
}

impl Reject {
    pub(crate) fn code(self) -> &'static str {
        match self {
            Reject::Oversized => "oversized",
            Reject::Parse => "parse",
            Reject::Signature => "signature",
            Reject::Expired => "expired",
            Reject::Replay => "replay",
            Reject::Schema => "schema",
        }
    }
}

/// A public key the app trusts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Key {
    pub(crate) kid: String,
    pub(crate) public: [u8; 32],
}

/// Canonical base64url without padding: what re-encodes to the same text.
/// Padding, other alphabets and non-zero trailing bits all fail that.
fn b64url(text: &str) -> Option<Vec<u8>> {
    let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let bytes = engine.decode(text).ok()?;
    (engine.encode(&bytes) == text).then_some(bytes)
}

fn signature_ok(key: &Key, message: &[u8], sig: &[u8]) -> bool {
    use openssl::pkey::{Id, PKey};
    use openssl::sign::Verifier;
    PKey::public_key_from_raw_bytes(&key.public, Id::ED25519)
        .and_then(|pkey| Verifier::new_without_digest(&pkey)?.verify_oneshot(sig, message))
        .unwrap_or(false)
}

/// The whole check, in the one order: size, envelope (exactly `v` 1, `kid`,
/// `payload`, `sig`), key by `kid`, canonical base64url, strict Ed25519 over
/// the payload bytes as served, and only then the payload: schema, issue time
/// (not more than an hour ahead), a validity of 0 < expires - issuedAt <= 22
/// days, not expired, and not older than `last_issued_at` (a replayed
/// manifest). Rollout 0 is a pause for everything, rollouts 1..=99 do not
/// bind a manual check or a Required update.
pub(crate) fn verify(
    body: &[u8],
    keys: &[Key],
    now: u64,
    last_issued_at: u64,
) -> Result<Payload, Reject> {
    if body.len() > MAX_MANIFEST_BYTES {
        return Err(Reject::Oversized);
    }
    let envelope = strict_json::parse(body).ok_or(Reject::Parse)?;
    // The shape first: exactly `v`, `kid`, `payload`, `sig`, `v` the integer
    // 1 and `kid` a string; anything else is a different document.
    let object = envelope.as_object().ok_or(Reject::Parse)?;
    let shaped = object.len() == 4
        && ["v", "kid", "payload", "sig"]
            .iter()
            .all(|k| object.contains_key(*k))
        && object["v"].as_i64() == Some(1);
    let kid = object.get("kid").and_then(|k| k.as_str());
    let (true, Some(kid)) = (shaped, kid) else {
        return Err(Reject::Parse);
    };
    // An unknown key id is decided before `payload` and `sig` are looked at;
    // a known one with either not a string is a signature failure too.
    let key = keys
        .iter()
        .find(|key| key.kid == kid)
        .ok_or(Reject::Signature)?;
    let (Some(payload_b64), Some(sig_b64)) = (object["payload"].as_str(), object["sig"].as_str())
    else {
        return Err(Reject::Signature);
    };
    let payload = b64url(payload_b64).ok_or(Reject::Signature)?;
    let sig = b64url(sig_b64).ok_or(Reject::Signature)?;
    if sig.len() != 64 || !scalar_below_l(&sig[32..]) || !signature_ok(key, &payload, &sig) {
        return Err(Reject::Signature);
    }
    parse_payload(&payload, now, last_issued_at)
}

/// The Ed25519 group order `L`, little-endian.
const L: [u8; 32] = [
    0xed, 0xd3, 0xf5, 0x5c, 0x1a, 0x63, 0x12, 0x58, 0xd6, 0x9c, 0xf7, 0xa2, 0xde, 0xf9, 0xde, 0x14,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x10,
];

/// Strict Ed25519 (RFC 8032 section 5.1.7): the `S` half of a signature must
/// be below `L`, so a signature has one valid form and cannot be malleated.
fn scalar_below_l(s: &[u8]) -> bool {
    s.len() == 32
        && s.iter()
            .rev()
            .zip(L.iter().rev())
            .find(|(a, b)| a != b)
            .is_some_and(|(a, b)| a < b)
}

fn parse_payload(bytes: &[u8], now: u64, last_issued_at: u64) -> Result<Payload, Reject> {
    let value = strict_json::parse(bytes).ok_or(Reject::Parse)?;
    if !value.is_object() {
        return Err(Reject::Parse);
    }
    let number = |name: &str| {
        value
            .get(name)
            .and_then(|v| v.as_i64())
            .and_then(|v| u64::try_from(v).ok())
    };
    let schema = value.get("schemaVersion").and_then(|v| v.as_number());
    match schema.and_then(|n| n.as_i64()) {
        Some(1) => {}
        Some(newer) if newer > 1 => return Err(Reject::Schema),
        // An integer too large for 64 bits is a newer schema, not noise.
        None if schema.is_some_and(|n| n.is_f64()) => return Err(Reject::Schema),
        _ => return Err(Reject::Parse),
    }
    let (issued_at, expires) = match (number("issuedAt"), number("expires")) {
        (Some(issued), Some(expires)) => (issued, expires),
        _ => return Err(Reject::Parse),
    };
    if now > expires || expires <= issued_at || expires - issued_at > MAX_VALIDITY_SECS {
        // Out of date, or a validity that is not 0 < expires - issuedAt <= 22
        // days: a stolen old manifest cannot be given a longer life by its
        // terms.
        return Err(Reject::Expired);
    }
    if issued_at > now.saturating_add(FUTURE_SLACK_SECS) {
        // A clock that disagrees by more than an hour: treated like an expired
        // manifest (nothing offered, nothing stored).
        return Err(Reject::Expired);
    }
    if issued_at < last_issued_at {
        return Err(Reject::Replay);
    }
    let channel = |name: &str| {
        value
            .get("channels")
            .and_then(|channels| channels.get(name))
            .and_then(|channel| channel.get("desktop"))
            .and_then(parse_component)
    };
    Ok(Payload {
        issued_at,
        expires,
        beta: channel("beta"),
        stable: channel("stable"),
    })
}

/// Unicode format characters (category Cf): bidi controls and overrides,
/// zero-width characters, joiners, the byte order mark, tag characters.
fn is_format(c: char) -> bool {
    matches!(c,
        '\u{ad}' | '\u{600}'..='\u{605}' | '\u{61c}' | '\u{6dd}' | '\u{70f}' | '\u{890}'..='\u{891}' | '\u{8e2}'
        | '\u{180e}' | '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}'
        | '\u{2060}'..='\u{2064}' | '\u{2066}'..='\u{206f}' | '\u{feff}'
        | '\u{fff9}'..='\u{fffb}' | '\u{110bd}' | '\u{110cd}' | '\u{13430}'..='\u{13440}'
        | '\u{1bca0}'..='\u{1bca3}' | '\u{1d173}'..='\u{1d17a}' | '\u{e0001}' | '\u{e0020}'..='\u{e007f}')
}

/// Allowed text of a note or a required reason: 1 to `max` Unicode scalars
/// and none of Cc (NUL, newline, tab), Cf (bidi controls, zero-width, BOM),
/// Zl, Zp, or the markup characters `<`, `>` and `&`. Text that fails is
/// refused (the note is skipped, the reason ignored), never cleaned up. Text
/// that passes is still shown as plain text only (`use_markup(false)`).
pub(crate) fn text_ok(text: &str, max: usize) -> bool {
    let count = text.chars().count();
    (1..=max).contains(&count)
        && !text.chars().any(|c| {
            c.is_control() || is_format(c) || matches!(c, '\u{2028}' | '\u{2029}' | '<' | '>' | '&')
        })
}

/// `YYYY-MM-DD` that is a real calendar date.
fn real_date(text: &str) -> bool {
    let bytes = text.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return false;
    }
    let digits = |r: std::ops::Range<usize>| -> Option<u32> {
        bytes[r.clone()]
            .iter()
            .all(u8::is_ascii_digit)
            .then(|| text[r].parse().ok())
            .flatten()
    };
    let (Some(year), Some(month), Some(day)) = (digits(0..4), digits(5..7), digits(8..10)) else {
        return false;
    };
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return false,
    };
    year >= 1 && (1..=days).contains(&day)
}

const REPO: &str = "/derpx06/DeskLink";

/// Which link a manifest field is: the release tag page, or the tarball.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum LinkField {
    ReleasePage,
    Tarball,
}

/// A manifest link, by exact shape (never "any path on a host"): `https`
/// (any letter case), host `github.com` (any case), an explicit port only
/// 443, no query, fragment, `%`, backslash, space, control or non-ASCII, no
/// empty, `.` or `..` segment, no trailing-dot host, and one of
///
/// * `/derpx06/DeskLink/releases/tag/v<version>` (a release page), or
/// * `/derpx06/DeskLink/releases/download/v<version>/desklink-<version>-x86_64.tar.gz`
///
/// where `<version>` is the component's own version. The desktop build knows
/// no other link: the app builds store and package links itself.
pub(crate) fn url_ok(link: &str, field: LinkField, version: &Version) -> bool {
    if link.len() > 300
        || !link.bytes().all(|b| (0x21..=0x7e).contains(&b))
        || link.contains('\\')
        || link.contains('%')
    {
        return false;
    }
    let Some(rest) = link
        .get(8..)
        .filter(|_| link[..8].eq_ignore_ascii_case("https://"))
    else {
        return false;
    };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(end);
    let host = authority.strip_suffix(":443").unwrap_or(authority);
    if host.is_empty()
        || host.ends_with('.')
        || !host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
        || !host.eq_ignore_ascii_case("github.com")
        || tail.contains(['?', '#'])
        || tail
            .split('/')
            .skip(1)
            .any(|segment| matches!(segment, "" | "." | ".."))
    {
        return false;
    }
    let want = match field {
        LinkField::ReleasePage => format!("{REPO}/releases/tag/v{version}"),
        LinkField::Tarball => {
            format!("{REPO}/releases/download/v{version}/desklink-{version}-x86_64.tar.gz")
        }
    };
    tail == want
}

fn sha256_hex(text: &str) -> Option<String> {
    (text.len() == 64 && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
        .then(|| text.to_string())
}

/// One desktop entry; `None` when it is not usable (no usable version, a
/// minimum above the version, a rollout outside 0..=100): the component is
/// dropped and nothing else changes. Cosmetic fields that are bad are dropped
/// one by one, never fatal.
fn parse_component(value: &serde_json::Value) -> Option<Component> {
    let object = value.as_object()?;
    let string = |name: &str| object.get(name).and_then(|v| v.as_str());
    let version = Version::parse(string("version")?)?;
    let min_supported = match object.get("minSupported") {
        None => Version(0, 0, 0),
        Some(min) => Version::parse(min.as_str()?)?,
    };
    if min_supported > version {
        return None;
    }
    let rollout = match object.get("rollout") {
        None => 100,
        Some(rollout) => u8::try_from(rollout.as_i64().filter(|r| (0..=100).contains(r))?).ok()?,
    };
    let notes = object
        .get("notes")
        .and_then(|notes| notes.as_array())
        .map(|notes| {
            notes
                .iter()
                .filter_map(|note| {
                    let title = note
                        .get("title")?
                        .as_str()
                        .filter(|t| text_ok(t, MAX_TITLE))?;
                    let detail = match note.get("detail") {
                        None | Some(serde_json::Value::Null) => None,
                        Some(detail) => match detail.as_str()? {
                            "" => None,
                            text if text_ok(text, MAX_DETAIL) => Some(text.to_string()),
                            _ => return None,
                        },
                    };
                    let glyph =
                        Glyph::parse(note.get("glyph").and_then(|g| g.as_str()).unwrap_or(""));
                    Some(Note {
                        title: title.to_string(),
                        detail,
                        glyph,
                    })
                })
                .take(MAX_NOTES)
                .collect()
        })
        .unwrap_or_default();
    let downloads = object.get("downloads");
    let page = |text: Option<&str>| {
        text.filter(|link| url_ok(link, LinkField::ReleasePage, &version))
            .map(str::to_string)
    };
    let tarball = downloads.and_then(|d| d.get("tarball")).and_then(|t| {
        let url = t.get("url")?.as_str()?;
        url_ok(url, LinkField::Tarball, &version).then(|| Download {
            url: url.to_string(),
            sha256: t
                .get("sha256")
                .and_then(|s| s.as_str())
                .and_then(sha256_hex),
        })
    });
    Some(Component {
        version,
        released: string("released")
            .filter(|r| real_date(r))
            .map(str::to_string),
        min_supported,
        required_reason: string("requiredReason")
            .filter(|r| text_ok(r, MAX_REASON))
            .map(str::to_string),
        rollout,
        notes,
        notes_url: page(string("notesUrl")),
        release_url: page(
            downloads
                .and_then(|d| d.get("releaseUrl"))
                .and_then(|r| r.as_str()),
        ),
        tarball,
    })
}

/// The release this channel offers. Beta also looks at stable and takes it
/// only when it is newer; stable looks at stable alone. A channel with
/// nothing in it offers nothing.
pub(crate) fn select(payload: &Payload, channel: Channel) -> Option<&Component> {
    match channel {
        Channel::Stable => payload.stable.as_ref(),
        Channel::Beta => match (&payload.beta, &payload.stable) {
            (Some(beta), Some(stable)) if stable.version > beta.version => Some(stable),
            (Some(beta), _) => Some(beta),
            (None, stable) => stable.as_ref(),
        },
    }
}

/// 0..=99, from the local install id (16 raw bytes) and the release, never
/// from anything that leaves the device: the first 4 bytes of
/// `sha256(id || "|" || component || "|" || version)`, big-endian, mod 100.
pub(crate) fn bucket(install_id: &[u8], component: &str, version: &Version) -> u32 {
    let mut input = install_id.to_vec();
    input.extend_from_slice(format!("|{component}|{version}").as_bytes());
    let digest = openssl::sha::sha256(&input);
    u32::from_be_bytes([digest[0], digest[1], digest[2], digest[3]]) % 100
}

/// What the person has chosen about one version.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Prefs {
    pub(crate) skipped: Option<Version>,
    pub(crate) snoozed: Option<Version>,
    /// Unix seconds.
    pub(crate) snoozed_until: u64,
}

/// What the manifest means for this install (the manifest-derived half of
/// the states in section 4.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Decision {
    UpToDate,
    Available(Component),
    /// Installed is below `minSupported`: ignores skip, snooze and rollout.
    Required(Component),
    Skipped(Component),
    Snoozed(Component, u64),
}

impl Decision {
    /// The offered release, when there is one in any state.
    pub(crate) fn component(&self) -> Option<&Component> {
        match self {
            Decision::UpToDate => None,
            Decision::Available(c)
            | Decision::Required(c)
            | Decision::Skipped(c)
            | Decision::Snoozed(c, _) => Some(c),
        }
    }
}

/// Section 4.3, in its order. `manual` (the person pressed Check for
/// updates) ignores rollout, skip and snooze; `Required` ignores them always.
pub(crate) fn decide(
    installed: Version,
    channel: Channel,
    payload: &Payload,
    prefs: &Prefs,
    install_id: &[u8],
    now: u64,
    manual: bool,
) -> Decision {
    let Some(component) = select(payload, channel) else {
        return Decision::UpToDate;
    };
    // Rollout 0 is the publisher's pause: nothing is shown, not even for a
    // manual check or a Required update.
    if component.rollout == 0 {
        return Decision::UpToDate;
    }
    if installed < component.min_supported {
        return Decision::Required(component.clone());
    }
    if component.version <= installed {
        return Decision::UpToDate;
    }
    if !manual && bucket(install_id, "desktop", &component.version) >= u32::from(component.rollout)
    {
        return Decision::UpToDate;
    }
    if !manual && prefs.skipped == Some(component.version) {
        return Decision::Skipped(component.clone());
    }
    if !manual && prefs.snoozed == Some(component.version) && prefs.snoozed_until > now {
        return Decision::Snoozed(component.clone(), prefs.snoozed_until);
    }
    Decision::Available(component.clone())
}

/// What to remember as the newest accepted issue time: never ahead of this
/// clock, so a manifest issued an hour "in the future" cannot make the honest
/// ones after it look like replays.
pub(crate) fn ratchet(issued_at: u64, now: u64) -> u64 {
    issued_at.min(now)
}
