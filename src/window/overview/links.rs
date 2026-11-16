//! Sending a link to the phone: the dialog and what counts as a link (hosts,
//! file endings). `link_to_send` is pure and unit-tested.
//!
//! Sibling edges: none.

use super::*;

/// "Send a link": a URL to open on the phone.
pub(in crate::window) fn show_send_link_dialog(
    daemon: DaemonHandle,
    device_id: String,
    device_name: &str,
    parent: &impl IsA<gtk::Widget>,
) {
    let entry = adw::EntryRow::builder()
        .title("Link")
        .input_purpose(gtk::InputPurpose::Url)
        .activates_default(true)
        .build();
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();
    list.append(&entry);
    let dialog = adw::AlertDialog::new(
        Some("Send a Link"),
        Some(&format!("It opens on {device_name}.")),
    );
    dialog.add_responses(&[("cancel", "_Cancel"), ("send", "_Send")]);
    dialog.set_response_appearance("send", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("send"));
    dialog.set_close_response("cancel");
    dialog.set_response_enabled("send", false);
    dialog.set_extra_child(Some(&list));
    {
        let dialog = dialog.downgrade();
        entry.connect_changed(move |entry| {
            if let Some(dialog) = dialog.upgrade() {
                dialog.set_response_enabled("send", link_to_send(&entry.text()).is_some());
            }
        });
    }
    let toast_target = parent.as_ref().clone();
    dialog.choose(parent, gtk::gio::Cancellable::NONE, move |response| {
        if response != "send" {
            return;
        }
        if let Some(link) = link_to_send(&entry.text()) {
            daemon.send(DaemonCommand::SendShareText(device_id.clone(), link));
            toast_from(&toast_target, "Link sent");
        }
    });
}

/// Endings that name a file, not a site (`README.md`, `photo.jpg`), though many
/// are also country domains: a bare word with one of them is not a link.
const FILE_ENDINGS: [&str; 44] = [
    "md", "txt", "pdf", "doc", "docx", "odt", "xls", "xlsx", "ods", "csv", "ppt", "pptx", "rtf",
    "png", "jpg", "jpeg", "gif", "svg", "webp", "bmp", "heic", "mp3", "mp4", "mkv", "wav", "flac",
    "zip", "tar", "gz", "xz", "7z", "rs", "py", "js", "ts", "kt", "java", "sh", "json", "toml",
    "yml", "yaml", "log", "apk",
];

/// A host name: dot-separated labels of letters, digits and hyphens. A bare
/// word must look like a site (two labels, an alphabetic ending of two or
/// more letters, not a file ending); after `http(s)://` any such host does.
fn is_host(host: &str, bare: bool) -> bool {
    let labels: Vec<&str> = host.split('.').collect();
    let label_ok = |label: &&str| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    };
    if !labels.iter().all(label_ok) {
        return false;
    }
    if !bare {
        return true;
    }
    let ending = labels
        .last()
        .copied()
        .unwrap_or_default()
        .to_ascii_lowercase();
    labels.len() >= 2
        && ending.len() >= 2
        && ending.bytes().all(|b| b.is_ascii_alphabetic())
        && !FILE_ENDINGS.contains(&ending.as_str())
}

/// What the user typed as a link to open: an `http://` or `https://` address,
/// or a bare `host.tld` (with an optional port and path) that gets `https://`.
/// A file name (`README.md`), another scheme (`file:///…`, `javascript:`,
/// `mailto:`), a name with spaces, or a host with `user@` is not a link.
pub(crate) fn link_to_send(typed: &str) -> Option<String> {
    let typed = typed.trim();
    if typed.is_empty()
        || typed.len() > 2000
        || typed.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return None;
    }
    let lower = typed.to_ascii_lowercase();
    let (rest, bare) = if lower.starts_with("https://") {
        (&typed[8..], false)
    } else if lower.starts_with("http://") {
        (&typed[7..], false)
    } else if typed.contains("://") {
        return None;
    } else {
        (typed, true)
    };
    let authority = &rest[..rest.find(['/', '?', '#']).unwrap_or(rest.len())];
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => (host, Some(port)),
        None => (authority, None),
    };
    if port.is_some_and(|port| {
        port.is_empty() || port.len() > 5 || !port.bytes().all(|b| b.is_ascii_digit())
    }) || !is_host(host, bare)
    {
        return None;
    }
    Some(if bare {
        format!("https://{typed}")
    } else {
        typed.to_string()
    })
}
