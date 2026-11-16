//! Finding a setting: the rows of each group with their search words and
//! synonyms (`ROWS`, `GROUPS`), and filtering the groups by a query.
//!
//! Sibling edges: none.

use super::*;

/// Every settings row inside `widget`.
pub(super) fn rows_in(widget: &gtk::Widget, found: &mut Vec<adw::PreferencesRow>) {
    if let Some(row) = widget.downcast_ref::<adw::PreferencesRow>() {
        found.push(row.clone());
    }
    let mut child = widget.first_child();
    while let Some(next) = child {
        rows_in(&next, found);
        child = next.next_sibling();
    }
}

/// The widget name of a row whose visibility is its own state (the glide
/// arrangement follows the glide switch): the search leaves it alone.
pub(super) const OWN_VISIBILITY: &str = "own-visibility";

/// A markup string as the plain text it shows ("Privacy &amp; security" is
/// "Privacy & security"); text that is not markup is itself.
pub(super) fn plain(markup: &str) -> String {
    gtk::pango::parse_markup(markup, '\0')
        .map_or_else(|_| markup.to_string(), |(_, text, _)| text.to_string())
}

/// A group as the search sees it: the group, its rows, and which of them
/// were shown before any search (a row hidden on purpose stays hidden).
pub(super) struct Searchable {
    group: adw::PreferencesGroup,
    rows: Vec<(adw::PreferencesRow, bool)>,
    shown: bool,
}

pub(super) fn searchable(groups: &[adw::PreferencesGroup]) -> Vec<Searchable> {
    groups
        .iter()
        .map(|group| {
            let mut rows = Vec::new();
            rows_in(group.upcast_ref(), &mut rows);
            Searchable {
                group: group.clone(),
                rows: rows
                    .into_iter()
                    .filter(|row| row.widget_name() != OWN_VISIBILITY)
                    .map(|row| {
                        let shown = row.is_visible();
                        (row, shown)
                    })
                    .collect(),
                shown: group.is_visible(),
            }
        })
        .collect()
}

/// Shows only the rows (and groups) whose title, line or search words hold
/// every word of `query`; a group whose own title does shows whole. True
/// when anything shows.
pub(super) fn filter_groups(groups: &[Searchable], query: &str) -> bool {
    // Typed text against the plain words of the rows, not their markup.
    let query = query.trim().to_lowercase();
    let words: Vec<&str> = query.split_whitespace().collect();
    let all_in = |text: &str| words.iter().all(|word| text.contains(word));
    let mut any = false;
    for Searchable { group, rows, shown } in groups {
        let whole = query.is_empty() || all_in(&plain(&group.title()).to_lowercase());
        let mut matching = 0;
        for (row, was_shown) in rows {
            let title = plain(&row.title());
            let mut text = format!("{title} {}", keywords_of(&title)).to_lowercase();
            if let Some(subtitle) = row
                .downcast_ref::<adw::ActionRow>()
                .and_then(|action| action.subtitle())
            {
                text.push(' ');
                text.push_str(&plain(&subtitle).to_lowercase());
            }
            let matches = whole || all_in(&text);
            row.set_visible(*was_shown && matches);
            matching += usize::from(*was_shown && matches);
        }
        let visible = *shown && (matching > 0 || rows.is_empty());
        group.set_visible(visible);
        any |= visible;
    }
    any
}

/// One setting as Settings search and Find anything know it.
pub(in crate::window) struct SettingRow {
    /// An index into [`GROUPS`].
    pub(crate) group: usize,
    /// The row's title; `{phone}` is the phone's name.
    pub(crate) title: &'static str,
    /// Shown only with a paired phone.
    pub(crate) phone: bool,
    /// Words people type for it that the row does not say (never shown).
    pub(crate) words: &'static str,
}

const fn row(group: usize, title: &'static str, phone: bool, words: &'static str) -> SettingRow {
    SettingRow {
        group,
        title,
        phone,
        words,
    }
}

/// Every setting, group by group: the same titles as
/// `testdata/settings_rows.json` (a test keeps the two in step).
pub(in crate::window) const ROWS: [SettingRow; 34] = [
    row(0, "Keep this computer awake while a phone is connected", false, "sleep suspend lid closed battery power stay awake caffeine"),
    row(0, "Let this phone keep the computer awake", true, "sleep suspend lid battery power stay awake allow"),
    row(0, "Start at login", false, "startup start at login autostart boot launch open automatically restart"),
    row(0, "This computer's name", false, "rename name computer name hostname what phones see"),
    row(0, "Pair another phone…", false, "pair add new phone connect second device qr code tablet"),
    row(1, "Show {phone}'s notifications here", true, "alerts messages banners reply mirror popups"),
    row(1, "Command alerts", true, "terminal finished done long running command notify notifications"),
    row(1, "Send this computer's notifications to the phone", true, "forward desktop notifications alerts apps"),
    row(1, "Share Do Not Disturb with {phone}", true, "dnd silent quiet mute focus banners notifications"),
    row(2, "Share this computer's clipboard with phones", true, "copy paste sync clipboard text"),
    row(3, "Pause music during calls", true, "media player music pause stop audio sound ring phone call"),
    row(3, "Show calls on this computer", true, "phone call incoming answer decline ring bar notification missed calls"),
    row(3, "Show caller names on the lock screen", true, "privacy lock screen locked caller id name number hide show incoming call"),
    row(3, "Ring on this computer", true, "ring sound chime tone phone call volume quiet silent"),
    row(3, "Name callers from {phone}'s contacts", true, "caller id contacts names numbers people who is calling"),
    row(4, "Phones may hear this computer's microphone", true, "mic microphone audio voice record sound listen"),
    row(5, "Glide onto {phone}", true, "mouse keyboard pointer cursor trackpad touchpad edge move control screen edge"),
    row(5, "Let the phone control mouse and keyboard", true, "remote control share screen screen sharing mouse keyboard trackpad touchpad approve allow permissions"),
    row(6, "Bucket with {phone}", true, "shared folder files sync drop share"),
    row(6, "Downloads folder", true, "download location save where files go received folder open"),
    row(6, "Accept photos and screenshots from {phone}", true, "photos pictures images gallery camera screenshots automatic files"),
    row(7, "Ask before terminal and screen access", false, "password session password pin lock secure protect screen sharing terminal privacy"),
    row(7, "Terminal access", true, "terminal shell command line console allow permissions"),
    row(7, "Unlock with {phone} nearby", true, "unlock lock screen login password nearby proximity"),
    row(7, "Lock when I walk away with {phone}", true, "lock away leave walk screen lock security"),
    row(7, "Unpair {phone}…", true, "unpair forget remove phone disconnect delete pair"),
    row(8, "Style", false, "dark mode light theme night colours colors appearance"),
    row(8, "Tray icon", false, "status icon top bar panel system tray indicator"),
    row(8, "GNOME extension", false, "top bar menu panel quick settings lock screen"),
    row(8, "Phone in the top bar", false, "top bar panel indicator icon"),
    row(9, "Connection history", false, "log logs sessions past connections when activity"),
    row(9, "System check", false, "requirements install missing problems troubleshoot diagnose fix permissions"),
    row(9, "Crash reports", false, "logs bug report crash folder problems"),
    row(9, "About", false, "about version credits licence license developer new release notes problem"),
];

/// Whether `title` is `template` with the phone's name in place of `{phone}`.
pub(in crate::window) fn fits(template: &str, title: &str) -> bool {
    match template.split_once("{phone}") {
        Some((head, tail)) => {
            title.len() > head.len() + tail.len()
                && title.starts_with(head)
                && title.ends_with(tail)
        }
        None => template == title,
    }
}

/// What people type, and the setting it must find (Settings search and
/// Find anything both).
#[cfg(test)]
pub(in crate::window) const SYNONYMS: [(&str, &str); 33] = [
    (
        "battery",
        "Keep this computer awake while a phone is connected",
    ),
    ("notifications", "Show {phone}'s notifications here"),
    ("lock", "Lock when I walk away with {phone}"),
    ("unlock", "Unlock with {phone} nearby"),
    ("files", "Bucket with {phone}"),
    ("folder", "Downloads folder"),
    ("download location", "Downloads folder"),
    ("keyboard", "Let the phone control mouse and keyboard"),
    ("mouse", "Glide onto {phone}"),
    ("trackpad", "Glide onto {phone}"),
    ("screen", "Ask before terminal and screen access"),
    ("share screen", "Let the phone control mouse and keyboard"),
    ("dark mode", "Style"),
    ("sound", "Phones may hear this computer's microphone"),
    ("microphone", "Phones may hear this computer's microphone"),
    ("password", "Ask before terminal and screen access"),
    ("pair", "Pair another phone…"),
    ("unpair", "Unpair {phone}…"),
    ("forget", "Unpair {phone}…"),
    ("rename", "This computer's name"),
    ("startup", "Start at login"),
    ("start at login", "Start at login"),
    ("privacy", "Ask before terminal and screen access"),
    ("permissions", "Terminal access"),
    ("gallery", "Accept photos and screenshots from {phone}"),
    ("pictures", "Accept photos and screenshots from {phone}"),
    ("touchpad", "Glide onto {phone}"),
    ("copy paste", "Share this computer's clipboard with phones"),
    ("alerts", "Show {phone}'s notifications here"),
    ("calls", "Show calls on this computer"),
    ("shell", "Terminal access"),
    ("remove phone", "Unpair {phone}…"),
    ("contacts", "Name callers from {phone}'s contacts"),
];

/// The search words of the row titled `title` ("" for one not in [`ROWS`]).
fn keywords_of(title: &str) -> &'static str {
    ROWS.iter()
        .find(|row| fits(row.title, title))
        .map_or("", |row| row.words)
}

/// The groups of the Settings page, in the order the phone uses too:
/// Connection, Notifications, Clipboard, Calls, Sound, Control, Files,
/// Privacy & security, Appearance, Advanced. Only what people change is
/// here; everything else is a fixed default.
pub(in crate::window) const GROUPS: [&str; 10] = [
    "Connection",
    "Notifications",
    "Clipboard",
    "Calls",
    "Sound",
    "Control",
    "Files",
    "Privacy & security",
    "Appearance",
    "Advanced",
];
