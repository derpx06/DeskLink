// SPDX-License-Identifier: GPL-3.0-or-later
//! The About dialog: who makes DeskLink, what is new, where to report a
//! problem (with details to paste), and the credits and notices it owes.
//! derpx06 makes DeskLink; the KDE Connect authors are credited as what it
//! grew from, never as its makers, and nothing here claims KDE's endorsement.

use adw::prelude::*;
use gettextrs::gettext;

use crate::config::VERSION;
use crate::device_links::daemon::DaemonHandle;

const DEVELOPER: &str = "derpx06";
const WEBSITE: &str = "https://github.com/derpx06/DeskLink";
const ISSUES: &str = "https://github.com/derpx06/DeskLink/issues";
const PRIVACY: &str = "https://github.com/derpx06/DeskLink#privacy";

/// What changed in this version, in plain words (the dialog's own markup:
/// `<p>`, `<ul>` and `<li>` only).
const RELEASE_NOTES: &str = "<p>The first public release, a beta: a steadier connection, notes and a calmer Home.</p>\
<ul>\
<li>Says \u{201c}Checking connection\u{2026}\u{201d} when the phone goes quiet, and recovers or gives up within seconds.</li>\
<li>Comes back by itself after sleep, a new network or a restart.</li>\
<li>File transfers pick up where they stopped and never arrive twice.</li>\
<li>A Notes card on Home, and a layout that uses a big window.</li>\
<li>Glide, sound both ways, and the phone's notes, snippets, contacts and calls here when you turn them on.</li>\
<li>Pairing from the phone brings the connection up by itself.</li>\
</ul>";

/// The KDE Connect authors named in its own About data.
const KDE_CONNECT_AUTHORS: [&str; 5] = [
    "Albert Vaca Cintora",
    "Aleix Pol Gonzalez",
    "Nicolas Fella",
    "Simon Redman",
    "Everyone else who has contributed to KDE Connect",
];

/// Shows the dialog over `parent`; `daemon` gives the paired-phone count for the details.
pub(crate) fn present(parent: &impl IsA<gtk::Widget>, daemon: Option<&DaemonHandle>) {
    let paired_phones = daemon.map_or(0, |daemon| {
        daemon
            .devices()
            .iter()
            .filter(|device| device.trusted)
            .count()
    });
    let about = adw::AboutDialog::builder()
        .application_name("DeskLink")
        .application_icon("derx06.desklink.com")
        .developer_name(DEVELOPER)
        .version(VERSION)
        .release_notes_version(VERSION)
        .release_notes(RELEASE_NOTES)
        .comments("Your phone and your Linux computer, working together.")
        .website(WEBSITE)
        .issue_url(ISSUES)
        .license_type(gtk::License::Gpl30)
        .developers(vec![format!("{DEVELOPER} https://github.com/{DEVELOPER}")])
        // Translators: Replace "translator-credits" with your name/username, and optionally an email or URL.
        .translator_credits(gettext("translator-credits"))
        .copyright(format!("© 2026 {DEVELOPER}"))
        .debug_info(debug_info(
            paired_phones,
            // Read here, on the initialised GTK thread.
            [
                (
                    gtk::major_version(),
                    gtk::minor_version(),
                    gtk::micro_version(),
                ),
                (
                    adw::major_version(),
                    adw::minor_version(),
                    adw::micro_version(),
                ),
            ],
        ))
        .debug_info_filename("desklink-details.txt")
        .build();
    about.add_link("Privacy", PRIVACY);
    about.add_credit_section(Some("Based on KDE Connect"), &KDE_CONNECT_AUTHORS);
    about.add_legal_section(
        "KDE Connect",
        Some("© KDE Connect contributors"),
        gtk::License::Custom,
        Some("DeskLink grew out of KDE Connect, free software under the GNU General Public License, version 2 or 3. DeskLink is an independent project: it is not a KDE product and is not endorsed by KDE."),
    );
    about.add_legal_section(
        "JetBrains Mono",
        Some("© 2020 The JetBrains Mono Project Authors"),
        gtk::License::Custom,
        Some("The terminal font. SIL Open Font License, version 1.1: https://openfontlicense.org"),
    );
    about.add_legal_section(
        "Material Symbols",
        Some("© Google LLC"),
        gtk::License::Apache20,
        None,
    );
    about.add_legal_section(
        "Other libraries",
        None,
        gtk::License::Custom,
        Some("The Rust crates DeskLink is built from, each with its licence, are listed in THIRD_PARTY_LICENSES, installed with DeskLink."),
    );
    about.present(Some(parent));
}

/// The lines "Troubleshooting" offers to copy into a bug report: versions,
/// the kind of desktop and session, and a count of paired phones. No names,
/// addresses, ids or keys.
pub(crate) fn debug_info(paired_phones: usize, toolkit: [(u32, u32, u32); 2]) -> String {
    let [(gtk_major, gtk_minor, gtk_micro), (adw_major, adw_minor, adw_micro)] = toolkit;
    let (gst_major, gst_minor, gst_micro, _) = gst::version();
    let env = |key: &str| std::env::var(key).unwrap_or_else(|_| "unknown".into());
    format!(
        "DeskLink {VERSION}\n\
         GTK {gtk_major}.{gtk_minor}.{gtk_micro}\n\
         libadwaita {adw_major}.{adw_minor}.{adw_micro}\n\
         GStreamer {gst_major}.{gst_minor}.{gst_micro}\n\
         Desktop: {}\n\
         Session: {}\n\
         Paired phones: {paired_phones}\n",
        env("XDG_CURRENT_DESKTOP"),
        env("XDG_SESSION_TYPE"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Exactly the version lines, the desktop, the session and a count:
    /// nothing that names a person, a computer or a phone.
    #[test]
    fn the_details_hold_versions_desktop_session_and_a_count_only() {
        let info = debug_info(2, [(4, 18, 1), (1, 8, 0)]);
        let keys: Vec<&str> = info
            .lines()
            .map(|line| line.split([' ', ':']).next().unwrap())
            .collect();
        assert_eq!(
            keys,
            [
                "DeskLink",
                "GTK",
                "libadwaita",
                "GStreamer",
                "Desktop",
                "Session",
                "Paired"
            ]
        );
        assert!(info.starts_with(&format!("DeskLink {VERSION}\n")));
        assert!(info.ends_with("Paired phones: 2\n"));
    }

    /// The release notes use only the markup the dialog accepts, and are
    /// the 0.3 notes: a version bump fails here until they are rewritten.
    #[test]
    fn the_release_notes_are_markup_the_dialog_accepts() {
        let tags: Vec<&str> = RELEASE_NOTES
            .split('<')
            .skip(1)
            .map(|tag| tag.split('>').next().unwrap().trim_start_matches('/'))
            .collect();
        assert!(
            tags.iter().all(|tag| ["p", "ul", "li"].contains(tag)),
            "{tags:?}"
        );
        assert!(VERSION.starts_with("0.3"));
    }

    /// One version, in every place a person or a package manager reads it:
    /// `Cargo.toml`, `meson.build`, the newest AppStream release, the
    /// Debian changelog and the CHANGELOG's top heading all agree (they
    /// drifted before). The flake and the packaging files are checked too.
    #[test]
    fn the_version_is_the_same_everywhere_it_is_written() {
        let cargo = include_str!("../Cargo.toml");
        let version = cargo
            .lines()
            .find_map(|line| line.strip_prefix("version = \""))
            .and_then(|rest| rest.split('"').next())
            .expect("Cargo.toml version");
        assert_eq!(version, VERSION);
        let meson = include_str!("../meson.build");
        assert!(
            meson.contains(&format!("version: '{version}'")),
            "meson.build"
        );
        let metainfo = include_str!("../data/derx06.desklink.com.metainfo.xml.in");
        let release = metainfo
            .split("<release version=\"")
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .expect("a release");
        assert_eq!(release, version, "the newest AppStream release");
        let changelog = include_str!("../CHANGELOG.md");
        let top = changelog
            .lines()
            .find_map(|line| line.strip_prefix("## "))
            .expect("a heading");
        assert!(top.starts_with(version), "CHANGELOG top heading: {top}");
        assert!(include_str!("../packaging/debian/changelog")
            .starts_with(&format!("desklink ({version})")));
        assert!(include_str!("../flake.nix").contains(&format!("version = \"{version}\"")));
        assert!(include_str!("../packaging/obs/desklink.spec")
            .contains(&format!("Version:        {version}")));
        assert!(include_str!("../packaging/aur/desklink/PKGBUILD")
            .contains(&format!("pkgver={version}")));
        assert!(include_str!("../packaging/aur/desklink-bin/PKGBUILD")
            .contains(&format!("pkgver={version}")));
        assert!(include_str!("../derx06.desklink.com.json")
            .contains(&format!("\"tag\" : \"v{version}\"")));
        assert!(include_str!("../packaging/obs/_service").contains(&format!("v{version}</param>")));
        for srcinfo in [
            include_str!("../packaging/aur/desklink/.SRCINFO"),
            include_str!("../packaging/aur/desklink-bin/.SRCINFO"),
        ] {
            assert!(srcinfo.contains(&format!("pkgver = {version}")), ".SRCINFO");
            assert!(srcinfo.contains(&format!("v{version}")), ".SRCINFO source");
        }
        assert!(
            include_str!("../README.md").contains(&format!("desklink-{version}-x86_64.flatpak"))
        );
        assert!(include_str!("../flatpak/README.md").contains(&format!("v{version}")));
        // The AppStream release date and the Debian changelog's agree.
        let date = metainfo
            .split("<release version=\"")
            .nth(1)
            .and_then(|rest| rest.split("date=\"").nth(1))
            .and_then(|rest| rest.split('"').next())
            .expect("a release date");
        let (year, month, day) = {
            let mut parts = date.split('-');
            (
                parts.next().unwrap().to_string(),
                parts.next().unwrap().parse::<usize>().unwrap(),
                parts.next().unwrap().parse::<u32>().unwrap(),
            )
        };
        const MONTHS: [&str; 12] = [
            "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
        ];
        let debian = include_str!("../packaging/debian/changelog");
        assert!(
            debian.contains(&format!("{day} {} {year}", MONTHS[month - 1])),
            "the Debian changelog date is the AppStream release date ({date})"
        );
    }
}
