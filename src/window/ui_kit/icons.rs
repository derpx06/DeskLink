//! The kit's icons: Material Symbols Rounded (weight 400, grade 0, 24 px,
//! outlined), the same glyphs as the phone's `sym_<name>` drawables, as
//! symbolic SVGs `desklink-<name>-symbolic` in the resource bundle (GTK
//! recolours them; `scripts/import-material-symbols.py` writes them).
//! Apache-2.0, Google: see `THIRD_PARTY_LICENSES`.
//!
//! A name is written out in full where it is used
//! (`"desklink-home-symbolic"`); a test checks every such literal in `src/`
//! is in the bundle. Use GNOME names only for what the platform owns
//! (window controls, file chooser, `edit-copy`).
//!
//! Sizes: 16 inline in text, 18 in chips, 20 in buttons, 24 in rows, bars
//! and tiles; an empty state puts a 28 glyph in a 72 circle.

/// The design's icon names (`UI-REDESIGN.md` 2.6) and the file they are.
pub(crate) const DESIGN_NAMES: &[(&str, &str)] = &[
    ("home", "desklink-home-symbolic"),
    ("folder", "desklink-folder-symbolic"),
    ("note", "desklink-edit-note-symbolic"),
    ("bell", "desklink-notifications-symbolic"),
    ("terminal", "desklink-terminal-symbolic"),
    ("settings", "desklink-settings-symbolic"),
    ("person", "desklink-person-symbolic"),
    ("search", "desklink-search-symbolic"),
    ("plus", "desklink-add-symbolic"),
    ("close", "desklink-close-symbolic"),
    ("check", "desklink-check-symbolic"),
    ("chevron", "desklink-chevron-right-symbolic"),
    ("expand", "desklink-expand-more-symbolic"),
    ("more", "desklink-more-vert-symbolic"),
    ("file-upload", "desklink-upload-file-symbolic"),
    ("clipboard", "desklink-content-paste-symbolic"),
    ("screen", "desklink-desktop-windows-symbolic"),
    ("lock", "desklink-lock-symbolic"),
    ("ring", "desklink-ring-volume-symbolic"),
    ("link", "desklink-link-symbolic"),
    ("qr", "desklink-qr-code-scanner-symbolic"),
    ("laptop", "desklink-device-laptop-symbolic"),
    ("phone", "desklink-device-phone-symbolic"),
    ("wifi", "desklink-wifi-symbolic"),
    ("internet", "desklink-public-symbolic"),
    ("battery", "desklink-battery-full-symbolic"),
    ("sound", "desklink-volume-up-symbolic"),
    ("mic", "desklink-mic-symbolic"),
    ("camera", "desklink-photo-camera-symbolic"),
    ("scan", "desklink-document-scanner-symbolic"),
    ("trash", "desklink-delete-symbolic"),
    ("pin", "desklink-keep-symbolic"),
    ("reply", "desklink-reply-symbolic"),
    ("snooze", "desklink-snooze-symbolic"),
    ("copy", "desklink-content-copy-symbolic"),
    ("share", "desklink-share-symbolic"),
    ("pause", "desklink-pause-symbolic"),
    ("play", "desklink-play-arrow-symbolic"),
    ("stop", "desklink-stop-circle-symbolic"),
    ("warning", "desklink-warning-symbolic"),
    ("error", "desklink-error-symbolic"),
    ("info", "desklink-info-symbolic"),
    ("bucket", "desklink-inventory-symbolic"),
    ("folder-sync", "desklink-directory-sync-symbolic"),
];

/// The icon file for a design name ("bell" is `desklink-notifications-symbolic`).
pub(crate) fn design_icon(name: &str) -> Option<&'static str> {
    DESIGN_NAMES
        .iter()
        .find(|(design, _)| *design == name)
        .map(|(_, file)| *file)
}
