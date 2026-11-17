//! What the popup says, as plain data: its title, the two version chips, up to
//! three note rows with their glyph, the install source line and the
//! guidance. No widgets, so every state is tested without a display.
//!
//! Sibling edges: `words`. Outside: `crate::update` (the types it names).

use super::words;
use crate::update::{Component, Glyph, Guidance, Source, Version};

/// A note row: its icon, the bold line and the muted one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct NoteRow {
    pub(super) icon: &'static str,
    pub(super) title: String,
    pub(super) detail: Option<String>,
}

/// What the popup shows for one offer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Popup {
    /// The Required variant: only "How to update", no way to skip.
    pub(super) required: bool,
    pub(super) version: Version,
    pub(super) title: String,
    /// The installed and the offered version as shown ("0.3.0 (beta)").
    pub(super) from: String,
    pub(super) to: String,
    pub(super) notes: Vec<NoteRow>,
    /// The Required sentence (the manifest's own reason when it has one).
    pub(super) body: Option<String>,
    pub(super) installed_with: String,
    pub(super) source: Source,
    pub(super) release_url: Option<String>,
}

/// An icon from the bundle for each note glyph.
pub(super) fn glyph_icon(glyph: Glyph) -> &'static str {
    match glyph {
        Glyph::Refresh => "desklink-refresh-symbolic",
        Glyph::Swap => "desklink-directory-sync-symbolic",
        Glyph::Check => "desklink-check-symbolic",
        Glyph::Files => "desklink-folder-symbolic",
        Glyph::Terminal => "desklink-terminal-symbolic",
        Glyph::Speed => "desklink-history-symbolic",
        Glyph::Shield => "desklink-encrypted-symbolic",
        Glyph::Star => "desklink-badge-symbolic",
    }
}

/// "0.4.0 (beta)" on the beta channel, else "0.4.0".
pub(super) fn version_label(version: Version, beta: bool) -> String {
    if beta {
        format!("{version} (beta)")
    } else {
        version.to_string()
    }
}

pub(super) fn popup(
    component: &Component,
    installed: Version,
    beta: bool,
    required: bool,
    source: Source,
) -> Popup {
    let version = component.version;
    Popup {
        required,
        version,
        title: if required {
            words::REQUIRED_TITLE.to_string()
        } else {
            words::available_title(&version_label(version, beta))
        },
        from: version_label(installed, beta),
        to: version_label(version, beta),
        notes: component
            .notes
            .iter()
            .take(3)
            .map(|note| NoteRow {
                icon: glyph_icon(note.glyph),
                title: note.title.clone(),
                detail: note.detail.clone(),
            })
            .collect(),
        body: required.then(|| {
            component
                .required_reason
                .clone()
                .unwrap_or_else(|| words::REQUIRED_BODY.to_string())
        }),
        installed_with: words::installed_with(source.label()),
        source,
        release_url: component
            .release_url
            .clone()
            .or_else(|| component.notes_url.clone()),
    }
}

/// What "How to update" shows for this source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Guide {
    /// A command to copy, with the note under it.
    pub(super) command: Option<(&'static str, Option<&'static str>)>,
    /// A sentence (no command).
    pub(super) text: Option<&'static str>,
    /// A button that opens the release page.
    pub(super) open_page: bool,
    /// "Your distribution may not have {version} yet."
    pub(super) lag: Option<String>,
}

pub(super) fn guide(popup: &Popup) -> Guide {
    let lag = popup
        .source
        .may_lag()
        .then(|| words::may_not_have_yet(&popup.version.to_string()));
    match popup.source.guidance() {
        Guidance::Command { command, note } => Guide {
            command: Some((command, note)),
            text: None,
            open_page: false,
            lag,
        },
        Guidance::Text(text) => Guide {
            command: None,
            text: Some(text),
            open_page: false,
            lag,
        },
        Guidance::ReleasePage(text) => Guide {
            command: None,
            text: Some(text),
            open_page: true,
            lag,
        },
    }
}

/// The page "Open the download page" opens when the manifest gave none.
pub(super) const RELEASES_PAGE: &str = "https://github.com/derpx06/DeskLink/releases";
