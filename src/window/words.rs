//! The words the window uses for errors and devices: friendly error text,
//! the jargon list, device kinds and icons, the install hint.
//!
//! Sibling edges: none. Outside: nothing but std (pure text, tested in
//! `jargon_tests`).

const OFFLINE: &str = "This computer is offline. DeskLink reconnects when the network is back.";

pub(super) fn is_offline_message(message: &str) -> bool {
    message == OFFLINE
}

/// A daemon error in plain words, and whether it is a lasting state (shown
/// as a banner) rather than a passing failure (a toast). Unknown errors keep
/// their text, minus the "(os error N)" tail.
pub(super) fn friendly_error(raw: &str) -> (String, bool) {
    let lower = raw.to_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|needle| lower.contains(needle));
    // The settings save names itself (see `Config::save`); any other file
    // error is not about the settings.
    let saving = lower.contains(&crate::device_links::config::SAVE_FAILED.to_lowercase());
    let (message, lasting) = if has(&[
        "(os error 101)",
        "network is unreachable",
        "(os error 113)",
        "no route to host",
    ]) {
        (OFFLINE, true)
    } else if has(&["downgraded protocol"]) {
        (
            "That device runs an older DeskLink. Update DeskLink on it to connect.",
            true,
        )
    } else if has(&["trusted device certificate changed"]) {
        (
            "This phone's identity changed. If DeskLink was reinstalled on it, pair it again \
             by scanning the QR code: the old pairing is replaced.",
            true,
        )
    } else if has(&[
        "clocks of this computer and the phone",
        "expired desklink webrtc signaling",
    ]) {
        (
            "This computer's clock and the phone's clock are more than 5 minutes apart. Set the \
             clock of the phone or computer to automatic, then try again.",
            false,
        )
    } else if has(&["too many wrong qr", "malformed qr"]) {
        (
            "That QR code can't be used any more. Scan the new one shown on this computer.",
            false,
        )
    } else if has(&["too many links are waiting to pair"]) {
        (
            "Several devices are trying to pair at once. Wait a moment, then scan again.",
            false,
        )
    } else if has(&["qr pairing code expired"]) {
        (
            "That QR code expired. Scan the new one shown on this computer.",
            false,
        )
    } else if has(&["no qr pairing code is active"]) {
        (
            "Open the pairing QR code on this computer first, then scan it.",
            false,
        )
    } else if has(&["invalid qr pairing proof"]) {
        (
            "That QR code is not from this computer. Scan the one shown here.",
            false,
        )
    } else if has(&["gstreamer webrtc plugins"]) {
        // The start-up check's own words: what to install, kept whole.
        return (raw.to_string(), true);
    } else if saving && has(&["(os error 30)", "read-only file system"]) {
        (
            "DeskLink cannot save anything: its settings folder is read-only. Make your \
             home folder writable, then restart DeskLink.",
            true,
        )
    } else if saving && has(&["(os error 28)", "no space left on device"]) {
        (
            "The disk is full, so DeskLink cannot save its settings. Free some space.",
            true,
        )
    } else if has(&["(os error 30)", "read-only file system"]) {
        (
            "A folder DeskLink needs is on a read-only file system.",
            false,
        )
    } else if has(&["(os error 28)", "no space left on device"]) {
        ("The disk is full. Free some space.", false)
    } else if has(&["(os error 98)", "address already in use"]) {
        (
            "Another app is using DeskLink's network ports. Close other phone-link apps, \
             then restart DeskLink.",
            true,
        )
    } else if has(&["(os error 111)", "connection refused"]) {
        (
            "The device refused the connection. Make sure DeskLink is open on it.",
            false,
        )
    } else if has(&["(os error 110)", "timed out"]) {
        (
            "The connection timed out. DeskLink will keep trying.",
            false,
        )
    } else if has(&[
        "(os error 104)",
        "connection reset",
        "(os error 32)",
        "broken pipe",
    ]) {
        (
            "The connection was interrupted. DeskLink will reconnect.",
            false,
        )
    } else if has(&[
        "certificate verify failed",
        "peer certificate",
        "bad certificate",
        "bad_certificate",
        "certificate unknown",
        "certificate required",
        "certificate has expired",
        "self-signed certificate",
        "unknown ca",
        "unknown_ca",
        "unable to get local issuer",
    ]) {
        (
            "This phone's identity could not be checked. If DeskLink was reinstalled on it, \
             pair it again by scanning the QR code.",
            true,
        )
    } else if has(&[
        "handshake",
        "tls error",
        "tls alert",
        "ssl error",
        "ssl routines",
        "ssl handshake",
        "sslv3 alert",
        "unexpected eof",
        "unexpected end of file",
        "failed to fill whole buffer",
        "connection aborted",
        "connection closed",
        "early eof",
    ]) {
        (
            "The connection dropped while it was being set up. DeskLink will try again.",
            false,
        )
    } else if has(&[
        "failed to lookup",
        "name or service not known",
        "no such host",
        "temporary failure in name resolution",
        "could not resolve",
        "failed to resolve",
    ]) {
        (
            "DeskLink could not find the device on the network. It will keep looking.",
            false,
        )
    } else if has(&["(os error 13)", "(os error 1)", "permission denied"]) {
        (
            "DeskLink was not allowed to do that. Check its permissions.",
            false,
        )
    } else {
        let trimmed = match raw.rfind(" (os error ") {
            Some(at) if raw.ends_with(')') => &raw[..at],
            _ => raw,
        };
        // Text written for developers stays behind Details.
        if has_jargon(trimmed) {
            return (SOMETHING_WENT_WRONG.to_string(), false);
        }
        return (trimmed.to_string(), false);
    };
    (message.to_string(), lasting)
}

/// What the Home hero says about a connection error, and the technical text
/// for a "Details" disclosure when it differs. Everything shown on the hero
/// goes through here: nothing the system or a library said is shown as is.
pub(super) fn hero_error_words(raw: &str) -> (String, Option<String>) {
    let (words, _) = friendly_error(raw);
    // The table found nothing: the text is the library's own. It stays out of
    // the hero (it is behind Details) unless it already reads as a sentence.
    let own = words == raw.trim()
        || raw
            .rfind(" (os error ")
            .is_some_and(|at| words == raw[..at]);
    let readable = own && !looks_technical(raw);
    if readable || !own {
        let details = (words != raw.trim()).then(|| raw.trim().to_string());
        return (words, details);
    }
    (
        "Something went wrong with the connection. DeskLink will try again.".to_string(),
        Some(raw.trim().to_string()),
    )
}

/// What an error says when its own words are for developers (they stay
/// behind Details).
const SOMETHING_WENT_WRONG: &str = "Something went wrong. Open Details to see more.";

/// Words a person would not know. Shown text says "address", "network",
/// "connection" or the feature's name instead; the jargon test reads the
/// window's strings against the same lists.
pub(super) const JARGON: [&str; 50] = [
    "webrtc",
    "sdp",
    "pty",
    "tls",
    "ssl",
    "certificate",
    "certificates",
    "payload",
    "handshake",
    "daemon",
    "portal",
    "gstreamer",
    "pipewire",
    "codec",
    "bitrate",
    "ssrc",
    "mime",
    "uri",
    "ip address",
    "socket",
    "packet",
    "packets",
    "protocol",
    "mdns",
    "udp",
    "tcp",
    "d bus",
    "dbus",
    "xdg",
    "stun",
    "peer",
    "backend",
    "json",
    "hostname",
    "subnet",
    "ipv4",
    "ipv6",
    "plugin",
    "plugins",
    "signaling",
    "handover",
    "logind",
    "mpris",
    "encoder",
    "decoder",
    "vp8",
    "h 264",
    "rtp",
    "uuid",
    "websocket",
];

/// Jargon that is a word only in capitals ("turn" and "ice" are English).
pub(super) const JARGON_CAPS: [&str; 6] = ["ICE", "TURN", "LAN", "IP", "URL", "API"];

/// Whether `text` uses a word from [`JARGON`] or [`JARGON_CAPS`] (whole
/// words; "D-Bus" is "d bus", "H.264" is "h 264").
pub(super) fn has_jargon(text: &str) -> bool {
    let words = |text: &str| {
        let spaced: String = text
            .chars()
            .map(|c| if c.is_alphanumeric() { c } else { ' ' })
            .collect();
        format!(
            " {} ",
            spaced.split_whitespace().collect::<Vec<_>>().join(" ")
        )
    };
    let lower = words(&text.to_lowercase());
    let cased = words(text);
    JARGON
        .iter()
        .any(|term| lower.contains(&format!(" {term} ")))
        || JARGON_CAPS
            .iter()
            .any(|term| cased.contains(&format!(" {term} ")))
}

/// The end of a "Could not …" message: ": <reason>" when the reason reads
/// as plain words, else "." (the reason is logged instead).
pub(crate) fn because(error: &str) -> String {
    let error = error.trim();
    if error.is_empty() || looks_technical(error) || has_jargon(error) {
        eprintln!("[UI] Not shown to the user: {error}");
        ".".to_string()
    } else {
        format!(": {error}")
    }
}

/// Text a person would not write: library names, error codes, paths.
fn looks_technical(text: &str) -> bool {
    let lower = text.to_lowercase();
    text.contains("::")
        || text.contains(": ")
            && ["error", "failed", "errno", "code", "os error"]
                .iter()
                .any(|word| lower.contains(word))
        || lower.contains("os error")
        || lower.contains("panicked")
        || text.contains('/') && text.contains(".rs")
}

/// A device's glyph and the word for it, from the identity packet's type
/// ("phone", "tablet", "tv", "laptop", "desktop"). The glyphs are bundled in
/// the app's resources, so every distribution shows the same distinct ones.
pub(super) fn device_kind(device_type: &str) -> (&'static str, &'static str) {
    match device_type.to_lowercase().as_str() {
        "phone" | "smartphone" => ("desklink-device-phone-symbolic", "Phone"),
        "tablet" => ("desklink-device-tablet-symbolic", "Tablet"),
        "tv" | "television" => ("desklink-device-tv-symbolic", "TV"),
        "laptop" => ("desklink-device-laptop-symbolic", "Laptop"),
        "desktop" => ("desklink-device-desktop-symbolic", "Computer"),
        _ => ("desklink-device-generic-symbolic", "Device"),
    }
}

pub(crate) fn device_type_icon(device_type: &str) -> &'static str {
    device_kind(device_type).0
}

/// "phone" → "Phone", "tv" → "TV".
pub(crate) fn device_type_label(device_type: &str) -> String {
    device_kind(device_type).1.to_string()
}

/// "482913" → "482 913": two groups read aloud and compare more easily.
/// Said where a "Copy command" button holds what to install: the package
/// names stay behind the button, the words stay plain.
pub(super) const INSTALL_HINT: &str =
    "To install what is missing, copy the command and paste it into a terminal.";

pub(super) fn spaced_code(code: &str) -> String {
    let digits: Vec<char> = code.chars().collect();
    if digits.len() == 6 {
        format!(
            "{} {}",
            digits[..3].iter().collect::<String>(),
            digits[3..].iter().collect::<String>()
        )
    } else {
        code.to_string()
    }
}
