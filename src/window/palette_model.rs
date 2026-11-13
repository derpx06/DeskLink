//! "Find anything": what the palette lists and how a query ranks it. Pure
//! (no GTK, no daemon), so a test can say what a query finds and which
//! actions a phone is offered without drawing anything. The phone's
//! `PaletteModel.kt` is the reference: the same groups of results, the same
//! ranking (title prefix, then a word of the title, then anywhere in the
//! title, then the subtitle and keywords; every word of the query must
//! match), the same caps (6 a group when searching, 3 when idle; the phone
//! lists all its ~14 actions idle, the desk has twice as many so it lists
//! the first 6).
//!
//! `palette.rs` gathers the [`Sources`] from the live window, draws the
//! [`Item`]s and maps a chosen [`Target`] onto the window's existing actions
//! and commands; nothing here does anything.

use std::path::PathBuf;

use unicode_normalization::UnicodeNormalization;

use crate::device_links::device::DeviceView;
use crate::device_links::packet::{
    PACKET_TYPE_CLIPBOARD, PACKET_TYPE_FINDMYPHONE_REQUEST, PACKET_TYPE_LOCK_REQUEST,
    PACKET_TYPE_MOUSEPAD_REQUEST, PACKET_TYPE_PING, PACKET_TYPE_SHARE_REQUEST,
};

/// Rows a group shows for a query.
pub(crate) const FOUND_PER_GROUP: usize = 6;
/// Rows a group shows before anything is typed.
pub(crate) const IDLE_PER_GROUP: usize = 3;
/// Actions shown before anything is typed: the first of the list, which is
/// the Home tiles' order, so the rest (Insert, Sound, ...) wait for a query.
pub(crate) const IDLE_ACTIONS: usize = 6;
/// Longest a row's title is, in characters; a clipboard entry or a note's
/// first line can be a page.
const TITLE_CHARS: usize = 90;
/// How much of a note's body its row is found by.
const BODY_CHARS: usize = 400;

/// The groups of results, in the order they are listed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum Group {
    Actions,
    Pages,
    Phones,
    Terminals,
    Files,
    Notes,
    Snippets,
    Clipboard,
    Contacts,
    Settings,
}

impl Group {
    pub(crate) const ALL: [Group; 10] = [
        Group::Actions,
        Group::Pages,
        Group::Phones,
        Group::Terminals,
        Group::Files,
        Group::Notes,
        Group::Snippets,
        Group::Clipboard,
        Group::Contacts,
        Group::Settings,
    ];

    pub(crate) fn title(self) -> &'static str {
        match self {
            Self::Actions => "Actions",
            Self::Pages => "Go to",
            Self::Phones => "Phones",
            Self::Terminals => "Terminals",
            Self::Files => "Recent files",
            Self::Notes => "Notes",
            Self::Snippets => "Snippets",
            Self::Clipboard => "Clipboard history",
            Self::Contacts => "Contacts",
            Self::Settings => "Settings",
        }
    }

    /// Rows before anything is typed. Pages and settings are reached by
    /// name, and snippets, the clipboard and contacts are text that can be a
    /// password or a stranger's number: none of them is put on show just
    /// because the palette opened (a shared screen shows it too). A note shows
    /// its title only (`Item::private_subtitle`).
    fn idle_cap(self) -> usize {
        match self {
            Self::Actions => IDLE_ACTIONS,
            Self::Phones | Self::Terminals | Self::Files | Self::Notes => IDLE_PER_GROUP,
            Self::Pages | Self::Snippets | Self::Clipboard | Self::Contacts | Self::Settings => 0,
        }
    }
}

/// What an action does; `palette.rs` maps each onto the window's own action
/// or command, the one the same button runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    SendFiles,
    SendClipboard,
    SendLink,
    Ring,
    Nudge,
    Lock,
    PhoneScreen,
    /// A row of `insert::CAPTURES`.
    Insert(usize),
    /// `desk-sound`, `phone-mic` or `phone-sound`, switched.
    /// `desk-sound`, `phone-mic` or `phone-sound`, and whether it is turned
    /// on (not toggled): what the row said when it was drawn is what it does.
    Sound(&'static str, bool),
    HandOff,
    NewNote,
    NewTerminal,
    Dial,
    PairAnother,
    History,
    Shortcuts,
}

/// What choosing a row does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Target {
    Action(Action),
    /// An action on this phone: the one the row was drawn for, so a change
    /// of selection since then cannot send it to another.
    PhoneAction(String, Action),
    /// A name of `PAGES`, or the `clipboard` / `snippets` tab of Notes.
    Page(&'static str),
    /// A subpage of Settings (an index into `prefs::GROUPS`).
    Settings(usize),
    /// Make this phone the one every page is about.
    Phone(String),
    /// A shared terminal, opened at the desk.
    Terminal(String),
    File(PathBuf),
    Note(String),
    /// Text to copy.
    Snippet(String),
    Clip(String),
    Contact {
        device: String,
        uid: String,
    },
    /// An action that cannot run now: choosing it says why, with the fix.
    Blocked(super::requirements::Need),
}

/// One row.
#[derive(Clone, Debug)]
pub(crate) struct Item {
    pub(crate) target: Target,
    pub(crate) group: Group,
    pub(crate) title: String,
    pub(crate) subtitle: String,
    pub(crate) icon: &'static str,
    /// The shortcut that does the same, as a GTK accelerator.
    pub(crate) accel: Option<&'static str>,
    /// The subtitle is the person's own words (a note's first line): not
    /// drawn until something is typed, so opening the palette over a screen
    /// share shows no text of theirs.
    pub(crate) private_subtitle: bool,
    folded_title: String,
    folded_title_words: Vec<String>,
    folded_rest: String,
}

/// Lower case with the accents taken off: "Café" is "cafe", "Ünal" is "unal".
pub(crate) fn fold(text: &str) -> String {
    text.nfd()
        .filter(|c| !unicode_normalization::char::is_combining_mark(*c))
        .collect::<String>()
        .to_lowercase()
}

fn words_of(folded: &str) -> Vec<String> {
    folded
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_string)
        .collect()
}

/// A single line of at most `max` characters, a cut one ending in an
/// ellipsis. Only the first few times `max` characters are looked at: a
/// clipboard entry or a note can be megabytes.
pub(crate) fn one_line(text: &str, max: usize) -> String {
    let window = max.saturating_mul(4).max(max + 1);
    let head: String = text.chars().take(window).collect();
    let more = text.chars().nth(window).is_some();
    let line: String = head.split_whitespace().collect::<Vec<_>>().join(" ");
    if !more && line.chars().count() <= max {
        return line;
    }
    let mut cut: String = line.chars().take(max.saturating_sub(1)).collect();
    cut.truncate(cut.trim_end().len());
    cut.push('…');
    cut
}

impl Item {
    fn new(
        target: Target,
        group: Group,
        title: &str,
        subtitle: &str,
        icon: &'static str,
        keywords: &str,
    ) -> Self {
        let title = one_line(title, TITLE_CHARS);
        let folded_title = fold(&title);
        Self {
            folded_title_words: words_of(&folded_title),
            folded_rest: fold(&format!("{subtitle} {keywords}")),
            folded_title,
            target,
            group,
            title,
            subtitle: one_line(subtitle, 120),
            icon,
            accel: None,
            private_subtitle: false,
        }
    }

    fn private(mut self) -> Self {
        self.private_subtitle = true;
        self
    }

    fn accel(mut self, accel: &'static str) -> Self {
        self.accel = Some(accel);
        self
    }

    /// How well the item matches every word of the query (already folded):
    /// 0 the title starts with it, 1 a word of the title does, 2 the title
    /// contains it, 3 the subtitle or keywords do; summed over the words.
    /// `None` when any word matches nothing.
    fn score(&self, words: &[String]) -> Option<usize> {
        let mut total = 0;
        for word in words {
            total += if self.folded_title.starts_with(word.as_str()) {
                0
            } else if self
                .folded_title_words
                .iter()
                .any(|w| w.starts_with(word.as_str()))
            {
                1
            } else if self.folded_title.contains(word.as_str()) {
                2
            } else if self.folded_rest.contains(word.as_str()) {
                3
            } else {
                return None;
            };
        }
        Some(total)
    }
}

#[cfg(test)]
impl Item {
    /// A note row, for tests of what is drawn.
    pub(crate) fn probe_note(title: &str, preview: &str) -> Item {
        Item::new(
            Target::Note("n".into()),
            Group::Notes,
            title,
            preview,
            "desklink-edit-note-symbolic",
            "",
        )
        .private()
    }

    pub(crate) fn probe_snippet(label: &str, text: &str) -> Item {
        Item::new(
            Target::Snippet(text.into()),
            Group::Snippets,
            label,
            "Copy",
            "desklink-keep-symbolic",
            text,
        )
    }
}

/// A group of results: indices into the item list given to [`search`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Section {
    pub(crate) group: Group,
    pub(crate) items: Vec<usize>,
}

/// The sections for `query`: groups in [`Group`] order, each ranked (best
/// first, the given order among equals) and cut to [`FOUND_PER_GROUP`]. A
/// blank query lists the first [`IDLE_ACTIONS`] actions and [`IDLE_PER_GROUP`] of the groups that
/// show before anything is typed. Empty groups are left out, so no sections
/// means "Nothing matches".
pub(crate) fn search(items: &[Item], query: &str) -> Vec<Section> {
    let folded = fold(query);
    let words: Vec<String> = folded.split_whitespace().map(str::to_string).collect();
    Group::ALL
        .into_iter()
        .filter_map(|group| {
            let in_group = items
                .iter()
                .enumerate()
                .filter(|(_, item)| item.group == group);
            let shown: Vec<usize> = if words.is_empty() {
                in_group
                    .map(|(index, _)| index)
                    .take(group.idle_cap())
                    .collect()
            } else {
                let mut scored: Vec<(usize, usize)> = in_group
                    .filter_map(|(index, item)| item.score(&words).map(|score| (score, index)))
                    .collect();
                // Stable by index: equals keep the order they were given in.
                scored.sort();
                scored
                    .into_iter()
                    .take(FOUND_PER_GROUP)
                    .map(|(_, index)| index)
                    .collect()
            };
            (!shown.is_empty()).then_some(Section {
                group,
                items: shown,
            })
        })
        .collect()
}

/// How many rows the sections list.
pub(crate) fn count(sections: &[Section]) -> usize {
    sections.iter().map(|section| section.items.len()).sum()
}

/// What a screen reader is told when the list changes.
pub(crate) fn count_words(shown: usize, query_blank: bool) -> String {
    match (shown, query_blank) {
        (0, _) => "No results".to_string(),
        (1, true) => "1 suggestion".to_string(),
        (n, true) => format!("{n} suggestions"),
        (1, false) => "1 result".to_string(),
        (n, false) => format!("{n} results"),
    }
}

/// The row to select after a move of `by` rows from `current` in a list of
/// `len` (no wrapping: the ends stop). `None` when there is nothing to select.
pub(crate) fn step(current: Option<usize>, len: usize, by: isize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let from = current.unwrap_or(if by >= 0 { 0 } else { len - 1 }) as isize;
    // The first move from nothing lands on the first (or last) row.
    let target = if current.is_none() { from } else { from + by };
    Some(target.clamp(0, len as isize - 1) as usize)
}

// ── What a phone is offered ───────────────────────────────────────────────

/// The selected phone, as far as the palette cares.
#[derive(Clone, Debug, Default)]
pub(crate) struct PhoneState {
    pub(crate) id: String,
    pub(crate) name: String,
    /// "phone", "tablet", ... for the words of an action.
    pub(crate) kind: String,
    /// Reachable and its feature link ready: the Home tiles are live. What
    /// needs the phone is offered only then.
    pub(crate) usable: bool,
    /// Why not, in the words of the Home tiles' tooltips.
    pub(crate) reason: Option<String>,
    pub(crate) files: bool,
    pub(crate) clipboard: bool,
    pub(crate) ring: bool,
    pub(crate) ping: bool,
    pub(crate) lock: bool,
    /// The tiles of Phone screen, Insert and Sound share one capability.
    pub(crate) screen: bool,
    /// `insert::CAPTURES` rows this phone takes.
    pub(crate) captures: Vec<usize>,
    pub(crate) desk_sound_on: bool,
    pub(crate) phone_mic_on: bool,
    pub(crate) phone_sound_on: bool,
    /// What an action's needs are judged by (`requirements::blocker`).
    pub(crate) facts: super::requirements::PhoneFacts,
}

impl PhoneState {
    /// Reads what the Home tiles read: an action is listed when this build
    /// has it, and the same needs say when it cannot run.
    pub(crate) fn from_device(device: &DeviceView) -> Self {
        let can = |packet: &str| {
            super::FeatureRegistry::desktop()
                .outgoing_capabilities()
                .iter()
                .any(|capability| capability == packet)
        };
        let reason = super::unavailable_reason(device);
        let screen = can(PACKET_TYPE_MOUSEPAD_REQUEST);
        Self {
            id: device.id.clone(),
            name: device.name.clone(),
            kind: super::device_type_label(&device.device_type).to_lowercase(),
            usable: reason.is_none(),
            reason,
            files: can(PACKET_TYPE_SHARE_REQUEST),
            clipboard: can(PACKET_TYPE_CLIPBOARD),
            ring: can(PACKET_TYPE_FINDMYPHONE_REQUEST),
            ping: can(PACKET_TYPE_PING),
            lock: can(PACKET_TYPE_LOCK_REQUEST),
            screen,
            captures: if screen {
                super::insert::offered(&device.id)
                    .map(|(index, _)| index)
                    .collect()
            } else {
                Vec::new()
            },
            desk_sound_on: super::sound::is_on(device, "desk-sound"),
            phone_mic_on: super::sound::is_on(device, "phone-mic"),
            phone_sound_on: super::sound::is_on(device, "phone-sound"),
            facts: super::requirements::PhoneFacts::from_device(device),
        }
    }
}

/// What `action` needs from the phone, for `requirements::blocker`.
fn feature_of(action: Action) -> Option<super::requirements::Feature> {
    use super::requirements::Feature;
    Some(match action {
        Action::SendFiles | Action::SendLink => Feature::Send(PACKET_TYPE_SHARE_REQUEST),
        Action::SendClipboard => Feature::Send(PACKET_TYPE_CLIPBOARD),
        Action::PhoneScreen | Action::Insert(_) | Action::Sound(..) => {
            Feature::Send(PACKET_TYPE_MOUSEPAD_REQUEST)
        }
        Action::Ring => Feature::Send(PACKET_TYPE_FINDMYPHONE_REQUEST),
        Action::Lock => Feature::Send(PACKET_TYPE_LOCK_REQUEST),
        Action::Nudge => Feature::Send(PACKET_TYPE_PING),
        Action::HandOff => Feature::HandOff,
        _ => return None,
    })
}

/// An action that needs the phone, with the words it is listed by.
fn phone_action(phone: &PhoneState, action: Action) -> Option<Item> {
    let kind = phone.kind.as_str();
    let name = phone.name.as_str();
    let on = |text: &str| -> String { text.replace("{name}", name).replace("{kind}", kind) };
    let (title, subtitle, icon, keywords, accel): (String, String, &str, &str, Option<&str>) =
        match action {
            Action::SendFiles => (
                "Send files".into(),
                on("To {name}"),
                "desklink-upload-file-symbolic",
                "share upload transfer",
                Some("<control>o"),
            ),
            Action::SendClipboard => (
                "Send clipboard".into(),
                on("Copied text to {name}"),
                "desklink-content-paste-symbolic",
                "copy paste text",
                None,
            ),
            Action::SendLink => (
                "Send a link".into(),
                on("Opens on {name}"),
                "desklink-link-symbolic",
                "url web address share",
                None,
            ),
            Action::PhoneScreen => (
                format!("{} screen", capital(kind)),
                "See and control it".into(),
                "desklink-mobile-screen-share-symbolic",
                "mirror remote view control display",
                None,
            ),
            Action::Insert(index) => {
                let (_, _, label, icon) = super::insert::CAPTURES.get(index)?;
                (
                    sentence_case(label),
                    on("From {name}'s camera"),
                    icon_for_capture(icon),
                    "insert camera photo scan picture qr text ocr",
                    None,
                )
            }
            Action::Sound(what, turn_on) => {
                let (title, keywords) = match (what, turn_on) {
                    ("desk-sound", false) => (
                        on("Stop playing this computer's sound on {name}"),
                        "audio speakers stop",
                    ),
                    ("desk-sound", true) => {
                        (on("Play this computer's sound on {name}"), "audio speakers")
                    }
                    ("phone-mic", false) => {
                        (on("Stop using {name} as microphone"), "mic audio stop")
                    }
                    ("phone-mic", true) => (on("Use {name} as microphone"), "mic audio"),
                    (_, false) => (
                        on("Stop playing {name}'s sound here"),
                        "audio speakers stop",
                    ),
                    (_, true) => (on("Play {name}'s sound here"), "audio speakers"),
                };
                (
                    title,
                    "Sound".into(),
                    "desklink-volume-up-symbolic",
                    keywords,
                    None,
                )
            }
            Action::Ring => (
                format!("Ring {kind}"),
                "Find it".into(),
                "desklink-ring-volume-symbolic",
                "find my phone locate lost",
                None,
            ),
            Action::Nudge => (
                "Send a nudge".into(),
                "A quiet notification".into(),
                "desklink-vibration-symbolic",
                "ping buzz",
                None,
            ),
            Action::Lock => (
                format!("Lock {kind}"),
                "Lock its screen".into(),
                "desklink-lock-symbolic",
                "secure",
                None,
            ),
            Action::HandOff => (
                "Hand off".into(),
                on("Lock here, carry on from {name}"),
                "desklink-lock-symbolic",
                "walk away continue terminal",
                None,
            ),
            _ => return None,
        };
    let item = Item::new(
        Target::Action(action),
        Group::Actions,
        &title,
        &subtitle,
        icon,
        keywords,
    );
    Some(match accel {
        Some(accel) => item.accel(accel),
        None => item,
    })
}

fn capital(text: &str) -> String {
    let mut chars = text.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

/// "Take a Photo as a File" is "Take a photo as a file" here: rows of the
/// palette are sentence case.
fn sentence_case(text: &str) -> String {
    let mut words = text.split(' ');
    let first = words.next().unwrap_or_default().to_string();
    std::iter::once(first)
        .chain(words.map(|word| {
            if word.chars().all(|c| !c.is_lowercase()) && word.chars().count() > 1 {
                word.to_string() // PDF, QR
            } else {
                word.to_lowercase()
            }
        }))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The kit icon for a capture row (`insert::CAPTURES` names GNOME icons).
fn icon_for_capture(gnome: &str) -> &'static str {
    match gnome {
        "scanner-symbolic" => "desklink-document-scanner-symbolic",
        "format-text-plain-symbolic" => "desklink-description-symbolic",
        "view-grid-symbolic" => "desklink-qr-code-scanner-symbolic",
        _ => "desklink-photo-camera-symbolic",
    }
}

/// The actions to offer, in the order they are listed. What needs the phone
/// is listed whenever a phone is selected, also when it cannot run now (its
/// row then says what it needs); what is on this computer (notes,
/// terminals, pairing, the history, the shortcuts) is always there.
pub(crate) fn offered_actions(phone: Option<&PhoneState>) -> Vec<Action> {
    let mut out = Vec::new();
    // The Home tiles first, in their order: they are what is on show idle.
    if let Some(phone) = phone {
        if phone.files {
            out.push(Action::SendFiles);
        }
        if phone.clipboard {
            out.push(Action::SendClipboard);
        }
        if phone.files {
            out.push(Action::SendLink);
        }
        if phone.screen {
            out.push(Action::PhoneScreen);
        }
        if phone.ring {
            out.push(Action::Ring);
        }
        if phone.lock {
            out.push(Action::Lock);
        }
        if phone.ping {
            out.push(Action::Nudge);
        }
        out.push(Action::HandOff);
    }
    out.extend([Action::NewNote, Action::NewTerminal, Action::Dial]);
    // What is in the Insert and Sound menus: found by a query.
    if let Some(phone) = phone.filter(|phone| phone.screen) {
        out.extend(phone.captures.iter().map(|index| Action::Insert(*index)));
        // Each says what it does when chosen: on when it is off now.
        out.extend([
            Action::Sound("desk-sound", !phone.desk_sound_on),
            Action::Sound("phone-mic", !phone.phone_mic_on),
            Action::Sound("phone-sound", !phone.phone_sound_on),
        ]);
    }
    out.extend([Action::PairAnother, Action::History, Action::Shortcuts]);
    out
}

fn desk_action(action: Action) -> Item {
    let (title, subtitle, icon, keywords, accel) = match action {
        Action::NewNote => (
            "New note",
            "Write something down",
            "desklink-edit-note-symbolic",
            "write add",
            None,
        ),
        Action::NewTerminal => (
            "New terminal",
            "A shared shell on this computer",
            "desklink-terminal-symbolic",
            "shell console command line",
            None,
        ),
        Action::Dial => (
            "Dial a number",
            "Call with the phone, after asking",
            "desklink-call-symbolic",
            "phone call keypad dial pad",
            Some("<control><shift>d"),
        ),
        Action::PairAnother => (
            "Pair another phone",
            "Add a phone or tablet",
            "desklink-add-symbolic",
            "connect new device qr code",
            Some("<control>n"),
        ),
        Action::History => (
            "Connection history",
            "When your phones connected",
            "desklink-history-symbolic",
            "log connections",
            Some("<control>h"),
        ),
        _ => (
            "Keyboard shortcuts",
            "Every key that does something",
            "desklink-keyboard-symbolic",
            "keys hotkeys accelerators help",
            Some("<control>question"),
        ),
    };
    let item = Item::new(
        Target::Action(action),
        Group::Actions,
        title,
        subtitle,
        icon,
        keywords,
    );
    match accel {
        Some(accel) => item.accel(accel),
        None => item,
    }
}

// ── Everything else ───────────────────────────────────────────────────────

/// The pages, as `window::PAGES` has them, with the Ctrl+digit that opens
/// each (`PAGE_ACCELS`) and words that find it.
const PAGE_WORDS: [(&str, &str); 7] = [
    ("home", "overview start"),
    ("files", "transfers received sent bucket downloads"),
    ("notes", "snippets clipboard history"),
    ("notifications", "messages alerts"),
    ("terminals", "shell sessions"),
    ("contacts", "address book people"),
    ("settings", "preferences options"),
];

/// What the settings subpages are found by besides their title and summary.
const SETTINGS_WORDS: [&str; 10] = [
    "awake sleep login startup autostart",
    "alerts reply silence do not disturb forward",
    "copy paste share sync",
    "dial phone numbers music",
    "audio microphone speakers",
    "glide mouse keyboard remote control",
    "bucket downloads photos gallery folder",
    "password terminal access unlock unpair walk away security",
    "theme dark light accent tray icon top bar",
    if crate::device_links::relay_link::RELAY_ENABLED {
        "relay internet history logs checks away"
    } else {
        "history logs checks"
    },
];

/// One settings subpage as the palette lists it.
#[derive(Clone, Debug)]
pub(crate) struct SettingsPage {
    pub(crate) index: usize,
    pub(crate) title: &'static str,
    pub(crate) summary: &'static str,
    pub(crate) icon: &'static str,
}

/// A file in the transfer log.
#[derive(Clone, Debug)]
pub(crate) struct FileEntry {
    pub(crate) path: PathBuf,
    pub(crate) name: String,
    pub(crate) incoming: bool,
    pub(crate) device_name: String,
}

#[derive(Clone, Debug)]
pub(crate) struct NoteEntry {
    pub(crate) id: String,
    pub(crate) title: String,
    /// The first line of the body that is not the title.
    pub(crate) preview: String,
    pub(crate) body: String,
    pub(crate) pinned: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct ClipEntry {
    pub(crate) text: String,
    /// The phone it came from; `None` when copied here.
    pub(crate) from: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct ContactEntry {
    pub(crate) device: String,
    pub(crate) phone: String,
    pub(crate) uid: String,
    pub(crate) name: String,
    pub(crate) detail: Option<String>,
    /// Numbers (also bare digits), emails and company: found by, not shown.
    pub(crate) words: String,
}

#[derive(Clone, Debug)]
pub(crate) struct TerminalEntry {
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) cwd: Option<String>,
}

/// Everything the palette can list, as plain data.
#[derive(Clone, Debug, Default)]
pub(crate) struct Sources {
    /// A paired phone is selected: the pages of a phone exist.
    pub(crate) paired: bool,
    pub(crate) phone: Option<PhoneState>,
    /// The other paired phones: id, name, icon, a line about it.
    pub(crate) others: Vec<(String, String, &'static str, String)>,
    pub(crate) terminals: Vec<TerminalEntry>,
    pub(crate) files: Vec<FileEntry>,
    pub(crate) notes: Vec<NoteEntry>,
    /// Snippets: label, text.
    pub(crate) snippets: Vec<(String, String)>,
    pub(crate) clips: Vec<ClipEntry>,
    pub(crate) contacts: Vec<ContactEntry>,
    pub(crate) settings: Vec<SettingsPage>,
    /// `window::PAGES` titles and icons, by name.
    pub(crate) pages: Vec<(&'static str, &'static str, &'static str)>,
}

/// Every item the palette can list for `sources`, in the order each group
/// shows them.
pub(crate) fn build(sources: &Sources) -> Vec<Item> {
    let mut items = Vec::new();

    // Actions. Open terminals decide Hand off.
    let any_terminal_open = !sources.terminals.is_empty();
    for action in offered_actions(sources.phone.as_ref()) {
        let phone_item = sources
            .phone
            .as_ref()
            .and_then(|phone| phone_action(phone, action));
        items.push(match (phone_item, &sources.phone) {
            (Some(mut item), Some(phone)) => {
                let need = feature_of(action).and_then(|feature| {
                    super::requirements::blocker(feature, &phone.facts, any_terminal_open)
                });
                match need {
                    Some(need) => {
                        item.subtitle = need.title.clone();
                        item.target = Target::Blocked(need);
                    }
                    None => item.target = Target::PhoneAction(phone.id.clone(), action),
                }
                item
            }
            _ => desk_action(action),
        });
    }

    // Pages. The pages of a phone only for a paired one (as Ctrl+1 to 6);
    // the notes are the user's own either way.
    for (index, (name, words)) in PAGE_WORDS.iter().enumerate() {
        let Some((_, title, icon)) = sources.pages.iter().find(|(page, _, _)| page == name) else {
            continue;
        };
        if !sources.paired && !matches!(*name, "notes" | "settings") {
            continue;
        }
        let item = Item::new(
            Target::Page(name),
            Group::Pages,
            title,
            "",
            icon,
            &format!("page go open {words}"),
        );
        items.push(match index {
            0..=5 => item.accel(PAGE_ACCELS_TEXT[index]),
            _ => item.accel("<control>comma"),
        });
    }
    // The two tabs of Notes that people look for by name.
    if sources.paired {
        for (tab, title, icon) in [
            ("snippets", "Snippets", "desklink-keep-symbolic"),
            (
                "clipboard",
                "Clipboard history",
                "desklink-content-paste-symbolic",
            ),
        ] {
            items.push(Item::new(
                Target::Page(tab),
                Group::Pages,
                title,
                "Notes & clipboard",
                icon,
                "page go open",
            ));
        }
    }

    for (id, name, icon, line) in &sources.others {
        items.push(Item::new(
            Target::Phone(id.clone()),
            Group::Phones,
            name,
            &format!("Switch to it · {line}"),
            icon,
            "switch computer device",
        ));
    }
    for terminal in &sources.terminals {
        items.push(Item::new(
            Target::Terminal(terminal.id.clone()),
            Group::Terminals,
            &terminal.title,
            &match &terminal.cwd {
                Some(folder) => format!("Open here · {folder}"),
                None => "Open here".to_string(),
            },
            "desklink-terminal-symbolic",
            "shell session",
        ));
    }
    for file in &sources.files {
        items.push(Item::new(
            Target::File(file.path.clone()),
            Group::Files,
            &file.name,
            &if file.incoming {
                format!("Received from {}", file.device_name)
            } else {
                format!("Sent to {}", file.device_name)
            },
            if file.incoming {
                "desklink-download-symbolic"
            } else {
                "desklink-upload-symbolic"
            },
            "file transfer",
        ));
    }
    for note in &sources.notes {
        let body: String = note.body.chars().take(BODY_CHARS).collect();
        items.push(
            Item::new(
                Target::Note(note.id.clone()),
                Group::Notes,
                &note.title,
                &note.preview,
                if note.pinned {
                    "desklink-keep-filled-symbolic"
                } else {
                    "desklink-edit-note-symbolic"
                },
                &body,
            )
            .private(),
        );
    }
    for (label, text) in &sources.snippets {
        let words: String = text.chars().take(BODY_CHARS).collect();
        items.push(Item::new(
            Target::Snippet(text.clone()),
            Group::Snippets,
            label,
            "Copy",
            "desklink-keep-symbolic",
            &format!("snippet pinned {words}"),
        ));
    }
    for clip in &sources.clips {
        items.push(Item::new(
            Target::Clip(clip.text.clone()),
            Group::Clipboard,
            &clip.text,
            &match &clip.from {
                Some(phone) => format!("Copy · from {phone}"),
                None => "Copy · from this computer".to_string(),
            },
            if clip.from.is_some() {
                "desklink-device-phone-symbolic"
            } else {
                "desklink-device-desktop-symbolic"
            },
            "clipboard copied",
        ));
    }
    for contact in &sources.contacts {
        items.push(Item::new(
            Target::Contact {
                device: contact.device.clone(),
                uid: contact.uid.clone(),
            },
            Group::Contacts,
            &contact.name,
            &match &contact.detail {
                Some(detail) => format!("{detail} · {}", contact.phone),
                None => contact.phone.clone(),
            },
            "desklink-person-symbolic",
            &contact.words,
        ));
    }
    for page in &sources.settings {
        let words = SETTINGS_WORDS.get(page.index).copied().unwrap_or("");
        items.push(Item::new(
            Target::Settings(page.index),
            Group::Settings,
            page.title,
            &format!("Settings · {}", page.summary),
            page.icon,
            &format!("setting preferences {words}"),
        ));
    }
    items
}

/// `window::PAGE_ACCELS`, by position: the palette shows the key next to a
/// page (a test keeps the two equal).
pub(crate) const PAGE_ACCELS_TEXT: [&str; 6] = [
    "<control>1",
    "<control>2",
    "<control>3",
    "<control>4",
    "<control>5",
    "<control>6",
];

/// The line under the search box: whose actions these are.
pub(crate) fn scope_line(paired: bool, phone: Option<&PhoneState>) -> String {
    match phone {
        Some(phone) if phone.usable => format!("Actions for {}", phone.name),
        Some(phone) => format!(
            "{} · each action says what it needs",
            phone
                .reason
                .clone()
                .unwrap_or_else(|| format!("{} is away", phone.name)),
        ),
        None if paired => "No phone is selected".to_string(),
        None => "Pair a phone to send it files and use its actions".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device_links::device::DeviceStatus;
    use crate::device_links::device_info::DeviceInfo;
    use crate::device_links::features::FeatureRegistry;

    fn device(name: &str, ready: bool, status: DeviceStatus) -> DeviceView {
        let info = DeviceInfo {
            id: format!("id-{name}"),
            name: name.to_string(),
            device_type: "phone".to_string(),
            protocol_version: 9,
            incoming_capabilities: FeatureRegistry::desktop().outgoing_capabilities().to_vec(),
            outgoing_capabilities: FeatureRegistry::desktop().incoming_capabilities().to_vec(),
            instance_id: None,
            app_version: None,
        };
        let mut device = DeviceView::from_info(&info, "192.168.1.41".to_string(), true);
        device.status = status;
        device.feature_transport_ready = ready;
        device
    }

    fn ready_phone() -> PhoneState {
        PhoneState::from_device(&device("Pixel 9", true, DeviceStatus::Paired))
    }

    fn item(group: Group, title: &str, subtitle: &str, keywords: &str) -> Item {
        Item::new(
            Target::Note(title.to_string()),
            group,
            title,
            subtitle,
            "desklink-edit-note-symbolic",
            keywords,
        )
    }

    fn titles<'a>(items: &'a [Item], sections: &[Section], group: Group) -> Vec<&'a str> {
        sections
            .iter()
            .filter(|section| section.group == group)
            .flat_map(|section| section.items.iter().map(|i| items[*i].title.as_str()))
            .collect()
    }

    #[test]
    fn accents_and_case_are_folded() {
        assert_eq!(fold("Café Ünal ÀÉÎ"), "cafe unal aei");
        assert_eq!(fold("ŁÓDŹ"), "łodz");
        let items = [item(Group::Notes, "Café notes", "", "")];
        for query in ["cafe", "CAFÉ", "Cafe", "café"] {
            assert_eq!(count(&search(&items, query)), 1, "{query}");
        }
        // An accent typed where the title has none.
        let items = [item(Group::Notes, "Cafe", "", "")];
        assert_eq!(count(&search(&items, "café")), 1);
    }

    #[test]
    fn every_word_must_match_in_any_order() {
        let items = [
            item(Group::Notes, "Quarterly budget review", "", ""),
            item(Group::Notes, "Budget", "", ""),
        ];
        let found = search(&items, "review budget");
        assert_eq!(
            titles(&items, &found, Group::Notes),
            ["Quarterly budget review"]
        );
        assert_eq!(count(&search(&items, "budget zebra")), 0);
        // Extra spaces are nothing.
        assert_eq!(count(&search(&items, "  budget   ")), 2);
    }

    #[test]
    fn title_prefix_beats_word_prefix_beats_contains_beats_hints() {
        let items = [
            item(Group::Notes, "Xplanet", "", ""),               // contains
            item(Group::Notes, "Zzz", "", "planet words"),       // keywords
            item(Group::Notes, "Big planet", "", ""),            // word prefix
            item(Group::Notes, "Planet earth", "", ""),          // title prefix
            item(Group::Notes, "Aaa", "the planet of apes", ""), // subtitle
        ];
        let found = search(&items, "planet");
        assert_eq!(
            titles(&items, &found, Group::Notes),
            ["Planet earth", "Big planet", "Xplanet", "Zzz", "Aaa"]
        );
    }

    #[test]
    fn equal_scores_keep_the_order_they_were_given_in() {
        let items = [
            item(Group::Notes, "Note b", "", ""),
            item(Group::Notes, "Note a", "", ""),
            item(Group::Notes, "Note c", "", ""),
        ];
        let found = search(&items, "note");
        assert_eq!(
            titles(&items, &found, Group::Notes),
            ["Note b", "Note a", "Note c"]
        );
    }

    #[test]
    fn a_group_shows_six_for_a_query_and_three_when_idle() {
        let many: Vec<Item> = (0..10)
            .map(|n| item(Group::Notes, &format!("Note {n}"), "", ""))
            .collect();
        assert_eq!(search(&many, "note")[0].items.len(), FOUND_PER_GROUP);
        assert_eq!(search(&many, "")[0].items.len(), IDLE_PER_GROUP);
        assert_eq!(search(&many, "   ")[0].items.len(), IDLE_PER_GROUP);
        // The best six, not the first six.
        let mut items: Vec<Item> = (0..8)
            .map(|n| item(Group::Notes, &format!("Xnote {n}"), "", ""))
            .collect();
        items.push(item(Group::Notes, "Note best", "", ""));
        let found = search(&items, "note");
        assert_eq!(found[0].items.len(), FOUND_PER_GROUP);
        assert_eq!(items[found[0].items[0]].title, "Note best");
    }

    #[test]
    fn idle_lists_the_first_actions_and_hides_what_people_did_not_ask_for() {
        let mut items: Vec<Item> = (0..12)
            .map(|n| item(Group::Actions, &format!("Do {n}"), "", ""))
            .collect();
        for group in [
            Group::Pages,
            Group::Clipboard,
            Group::Contacts,
            Group::Settings,
        ] {
            items.push(item(group, "Secret", "", ""));
        }
        items.push(item(Group::Terminals, "zsh", "", ""));
        let idle = search(&items, "");
        assert_eq!(idle[0].group, Group::Actions);
        assert_eq!(idle[0].items.len(), IDLE_ACTIONS, "the first actions");
        assert_eq!(idle[0].items[0], 0);
        // A query reaches the rest.
        assert_eq!(search(&items, "do")[0].items.len(), FOUND_PER_GROUP);
        let groups: Vec<_> = idle.iter().map(|s| s.group).collect();
        assert_eq!(groups, [Group::Actions, Group::Terminals]);
        // A query reaches all of them.
        assert_eq!(search(&items, "secret").len(), 4);
    }

    #[test]
    fn groups_come_in_the_listed_order_and_empty_ones_are_left_out() {
        let items = [
            item(Group::Settings, "Alpha settings", "", ""),
            item(Group::Actions, "Alpha action", "", ""),
            item(Group::Notes, "Alpha note", "", ""),
        ];
        let groups: Vec<_> = search(&items, "alpha").iter().map(|s| s.group).collect();
        assert_eq!(groups, [Group::Actions, Group::Notes, Group::Settings]);
        assert!(search(&items, "nothing like it").is_empty());
    }

    #[test]
    fn counts_are_said_in_words() {
        assert_eq!(count_words(0, false), "No results");
        assert_eq!(count_words(1, false), "1 result");
        assert_eq!(count_words(6, false), "6 results");
        assert_eq!(count_words(1, true), "1 suggestion");
        assert_eq!(count_words(9, true), "9 suggestions");
    }

    #[test]
    fn stepping_stops_at_the_ends_and_starts_at_the_first_or_last() {
        assert_eq!(step(None, 0, 1), None);
        assert_eq!(step(None, 5, 1), Some(0));
        assert_eq!(step(None, 5, -1), Some(4));
        assert_eq!(step(Some(0), 5, 1), Some(1));
        assert_eq!(step(Some(0), 5, -1), Some(0), "no wrapping");
        assert_eq!(step(Some(4), 5, 1), Some(4));
        assert_eq!(step(Some(1), 20, 8), Some(9));
        assert_eq!(step(Some(18), 20, 8), Some(19), "a page stops at the end");
        assert_eq!(step(Some(3), 20, -8), Some(0));
    }

    #[test]
    fn long_text_is_one_short_line() {
        assert_eq!(one_line("a\n  b\t c", 20), "a b c");
        let cut = one_line(&"word ".repeat(40), 20);
        assert_eq!(cut.chars().count(), 20);
        assert!(cut.ends_with('…'));
        assert!(!cut.contains("  "));
        // Titles are cut when the item is made.
        let long = item(Group::Notes, &"x".repeat(500), "", "");
        assert!(long.title.chars().count() <= TITLE_CHARS);
    }

    // ── The actions ───────────────────────────────────────────────────────

    /// The actions listed as unavailable, with what each needs.
    fn blocked_of(items: &[Item]) -> Vec<(String, String)> {
        items
            .iter()
            .filter_map(|item| match &item.target {
                Target::Blocked(need) => Some((item.title.clone(), need.title.clone())),
                _ => None,
            })
            .collect()
    }

    fn titles_of(items: &[Item], group: Group) -> Vec<String> {
        items
            .iter()
            .filter(|item| item.group == group)
            .map(|item| item.title.clone())
            .collect()
    }

    fn sources_with(phone: Option<PhoneState>, terminals: usize) -> Sources {
        Sources {
            paired: phone.is_some(),
            phone,
            terminals: (0..terminals)
                .map(|n| TerminalEntry {
                    id: format!("t{n}"),
                    title: format!("zsh {n}"),
                    cwd: None,
                })
                .collect(),
            pages: vec![
                ("home", "Home", "desklink-home-symbolic"),
                ("files", "Files", "desklink-folder-symbolic"),
                ("notes", "Notes & clipboard", "desklink-edit-note-symbolic"),
                (
                    "notifications",
                    "Notifications",
                    "desklink-notifications-symbolic",
                ),
                ("terminals", "Terminals", "desklink-terminal-symbolic"),
                ("contacts", "Contacts", "desklink-person-symbolic"),
                ("settings", "Settings", "desklink-settings-symbolic"),
            ],
            ..Sources::default()
        }
    }

    #[test]
    fn a_ready_phone_is_offered_what_its_build_takes() {
        let phone = ready_phone();
        let actions = titles_of(&build(&sources_with(Some(phone), 0)), Group::Actions);
        for wanted in [
            "Send files",
            "Send clipboard",
            "Send a link",
            "Phone screen",
            "Ring phone",
            "Send a nudge",
            "Lock phone",
            "New note",
            "New terminal",
            "Pair another phone",
        ] {
            assert!(
                actions.contains(&wanted.to_string()),
                "{wanted} in {actions:?}"
            );
        }
        assert!(
            actions.iter().any(|a| a.starts_with("Take a photo")),
            "the Insert rows: {actions:?}"
        );
        assert!(
            actions.iter().any(|a| a.contains("microphone")),
            "the Sound rows: {actions:?}"
        );
        assert!(
            actions.contains(&"Hand off".to_string()),
            "listed, with its need"
        );
    }

    /// Away or still connecting: every action that needs the phone is still
    /// listed, each saying why it cannot run now, and choosing one explains.
    #[test]
    fn actions_that_need_the_phone_say_why_when_it_is_away_or_still_connecting() {
        for (ready, status, why) in [
            (false, DeviceStatus::Unreachable, "Can't reach Pixel 9"),
            (false, DeviceStatus::Paired, "Connecting to Pixel 9…"),
        ] {
            let phone = PhoneState::from_device(&device("Pixel 9", ready, status.clone()));
            assert!(!phone.usable);
            let items = build(&sources_with(Some(phone.clone()), 1));
            let actions = titles_of(&items, Group::Actions);
            assert!(actions.contains(&"Send files".to_string()), "{actions:?}");
            let blocked = blocked_of(&items);
            assert!(blocked.len() >= 8, "{blocked:?}");
            for (title, need) in &blocked {
                assert_eq!(need, why, "{title} ({status:?})");
            }
            let send = items
                .iter()
                .find(|item| item.title == "Send files")
                .unwrap();
            assert_eq!(send.subtitle, why, "the row says it");
            for desk in ["New note", "New terminal", "Pair another phone"] {
                assert!(!blocked.iter().any(|(title, _)| title == desk), "{desk}");
            }
            let line = scope_line(true, Some(&phone));
            assert!(line.contains("Pixel 9"), "{line}");
            assert!(line.contains("says what it needs"), "{line}");
        }
    }

    /// What an older phone's build cannot take is listed and says to update
    /// it; nothing is sent from such a row.
    #[test]
    fn an_old_phone_is_offered_what_it_cannot_do_with_the_reason() {
        let mut old = device("Old phone", true, DeviceStatus::Paired);
        old.incoming_capabilities
            .retain(|capability| capability != PACKET_TYPE_LOCK_REQUEST);
        old.incoming_capabilities
            .retain(|capability| capability != PACKET_TYPE_MOUSEPAD_REQUEST);
        let phone = PhoneState::from_device(&old);
        let items = build(&sources_with(Some(phone), 0));
        let blocked = blocked_of(&items);
        let needs = |title: &str| {
            blocked
                .iter()
                .find(|(blocked, _)| blocked == title)
                .map(|(_, need)| need.clone())
        };
        let update = Some("Old phone's DeskLink can't do this yet".to_string());
        assert_eq!(needs("Lock phone"), update);
        assert_eq!(needs("Phone screen"), update);
        assert_eq!(
            needs("Use Old phone as microphone"),
            update,
            "sound goes with the screen"
        );
        assert_eq!(needs("Send files"), None);
        assert_eq!(needs("Ring phone"), None);
        assert!(items.iter().any(
            |item| item.title == "Send files" && matches!(item.target, Target::PhoneAction(..))
        ));
    }

    #[test]
    fn hand_off_needs_terminal_access_then_an_open_terminal() {
        let mut phone = ready_phone();
        let hand_off = |phone: &PhoneState, terminals| {
            blocked_of(&build(&sources_with(Some(phone.clone()), terminals)))
                .into_iter()
                .find(|(title, _)| title == "Hand off")
                .map(|(_, need)| need)
        };
        assert_eq!(
            hand_off(&phone, 1).as_deref(),
            Some("Needs terminal access for Pixel 9")
        );
        phone.facts.terminal_access = true;
        assert_eq!(hand_off(&phone, 1), None, "live with a terminal open");
        assert_eq!(
            hand_off(&phone, 0).as_deref(),
            Some("Needs an open shared terminal")
        );
    }

    #[test]
    fn sound_rows_say_stop_once_they_are_on() {
        let mut phone = ready_phone();
        phone.phone_mic_on = true;
        let actions = titles_of(&build(&sources_with(Some(phone), 0)), Group::Actions);
        assert!(
            actions.contains(&"Stop using Pixel 9 as microphone".to_string()),
            "{actions:?}"
        );
        assert!(actions.contains(&"Play Pixel 9's sound here".to_string()));
    }

    #[test]
    fn with_no_phone_only_this_computers_actions_and_pages_remain() {
        let items = build(&sources_with(None, 0));
        let actions = titles_of(&items, Group::Actions);
        assert_eq!(
            actions,
            [
                "New note",
                "New terminal",
                "Dial a number",
                "Pair another phone",
                "Connection history",
                "Keyboard shortcuts"
            ]
        );
        // Notes are the user's own; the pages of a phone are not there.
        assert_eq!(
            titles_of(&items, Group::Pages),
            ["Notes & clipboard", "Settings"]
        );
        assert!(scope_line(false, None).starts_with("Pair a phone"));
    }

    #[test]
    fn a_tablet_is_a_tablet_in_the_words() {
        let mut tablet = device("Tab S9", true, DeviceStatus::Paired);
        tablet.device_type = "tablet".to_string();
        let phone = PhoneState::from_device(&tablet);
        let actions = titles_of(&build(&sources_with(Some(phone), 0)), Group::Actions);
        assert!(actions.contains(&"Ring tablet".to_string()), "{actions:?}");
        assert!(actions.contains(&"Lock tablet".to_string()));
        assert!(actions.contains(&"Tablet screen".to_string()));
    }

    #[test]
    fn typing_finds_an_action_by_its_name_or_a_synonym() {
        let items = build(&sources_with(Some(ready_phone()), 1));
        let first = |query: &str| -> String {
            let found = search(&items, query);
            let section = found
                .first()
                .unwrap_or_else(|| panic!("nothing for {query}"));
            items[section.items[0]].title.clone()
        };
        assert_eq!(first("send fil"), "Send files");
        assert_eq!(first("ring"), "Ring phone");
        assert_eq!(first("lock"), "Lock phone");
        assert_eq!(first("find my"), "Ring phone", "by a keyword");
        assert_eq!(first("hand"), "Hand off");
        assert_eq!(first("shell"), "New terminal");
        assert_eq!(first("pair"), "Pair another phone");
    }

    #[test]
    fn pages_carry_the_keys_that_open_them() {
        let items = build(&sources_with(Some(ready_phone()), 0));
        let pages: Vec<_> = items
            .iter()
            .filter(|i| i.group == Group::Pages)
            .map(|i| (i.title.as_str(), i.accel))
            .collect();
        assert_eq!(pages[0], ("Home", Some("<control>1")));
        assert_eq!(pages[5], ("Contacts", Some("<control>6")));
        assert_eq!(pages[6], ("Settings", Some("<control>comma")));
        // The same keys the window binds.
        for (index, (_, accel)) in super::super::PAGE_ACCELS.iter().enumerate() {
            assert_eq!(PAGE_ACCELS_TEXT[index], *accel);
        }
    }

    #[test]
    fn every_source_is_listed_and_found() {
        let mut sources = sources_with(Some(ready_phone()), 1);
        sources.others = vec![(
            "id2".into(),
            "Galaxy Tab".into(),
            "desklink-device-tablet-symbolic",
            "Connected".into(),
        )];
        sources.files = vec![FileEntry {
            path: "/home/u/Downloads/invoice.pdf".into(),
            name: "invoice.pdf".into(),
            incoming: true,
            device_name: "Pixel 9".into(),
        }];
        sources.notes = vec![NoteEntry {
            id: "n1".into(),
            title: "Groceries".into(),
            preview: "milk, eggs".into(),
            body: "milk, eggs\nand fresh basil".into(),
            pinned: false,
        }];
        sources.snippets = vec![("ssh login".into(), "ssh user@example.org".into())];
        sources.clips = vec![ClipEntry {
            text: "https://example.org/a?b=1".into(),
            from: Some("Pixel 9".into()),
        }];
        sources.contacts = vec![ContactEntry {
            device: "id1".into(),
            phone: "Pixel 9".into(),
            uid: "u1".into(),
            name: "Zoë Ångström".into(),
            detail: Some("+49 170 1234567".into()),
            words: "+49 170 1234567 491701234567 zoe@example.org Acme".into(),
        }];
        sources.settings = vec![SettingsPage {
            index: 8,
            title: "Appearance",
            summary: "Style and tray icon",
            icon: "desklink-palette-symbolic",
        }];
        let items = build(&sources);
        for (query, target) in [
            ("galaxy", Target::Phone("id2".into())),
            ("zsh", Target::Terminal("t0".into())),
            (
                "invoice",
                Target::File("/home/u/Downloads/invoice.pdf".into()),
            ),
            ("grocer", Target::Note("n1".into())),
            ("basil", Target::Note("n1".into())),
            ("ssh", Target::Snippet("ssh user@example.org".into())),
            (
                "example.org/a",
                Target::Clip("https://example.org/a?b=1".into()),
            ),
            (
                "zoe",
                Target::Contact {
                    device: "id1".into(),
                    uid: "u1".into(),
                },
            ),
            (
                "angstrom",
                Target::Contact {
                    device: "id1".into(),
                    uid: "u1".into(),
                },
            ),
            (
                "491701234567",
                Target::Contact {
                    device: "id1".into(),
                    uid: "u1".into(),
                },
            ),
            (
                "acme",
                Target::Contact {
                    device: "id1".into(),
                    uid: "u1".into(),
                },
            ),
            ("dark", Target::Settings(8)),
            ("tray", Target::Settings(8)),
        ] {
            let found = search(&items, query);
            let hit = found
                .iter()
                .flat_map(|s| s.items.iter())
                .any(|i| items[*i].target == target);
            assert!(hit, "{query} should find {target:?}");
        }
    }

    #[test]
    fn idle_shows_recent_things_but_not_the_clipboard_or_contacts() {
        let mut sources = sources_with(Some(ready_phone()), 1);
        sources.clips = vec![ClipEntry {
            text: "hunter2".into(),
            from: None,
        }];
        sources.contacts = vec![ContactEntry {
            device: "d".into(),
            phone: "p".into(),
            uid: "u".into(),
            name: "Ann".into(),
            detail: None,
            words: String::new(),
        }];
        sources.files = (0..5)
            .map(|n| FileEntry {
                path: format!("/f{n}").into(),
                name: format!("f{n}.txt"),
                incoming: n % 2 == 0,
                device_name: "Pixel 9".into(),
            })
            .collect();
        let items = build(&sources);
        let idle = search(&items, "");
        let groups: Vec<_> = idle.iter().map(|s| s.group).collect();
        assert!(!groups.contains(&Group::Clipboard));
        assert!(!groups.contains(&Group::Contacts));
        assert_eq!(
            idle.iter()
                .find(|s| s.group == Group::Files)
                .unwrap()
                .items
                .len(),
            IDLE_PER_GROUP
        );
        // But a query finds the clipboard.
        assert_eq!(count(&search(&items, "hunter")), 1);
    }

    #[test]
    fn nothing_matches_gives_no_sections() {
        let items = build(&sources_with(Some(ready_phone()), 0));
        assert!(search(&items, "qqqqzzzz").is_empty());
        assert_eq!(count(&search(&items, "qqqqzzzz")), 0);
    }

    #[test]
    fn the_shortcuts_shown_are_the_ones_the_window_binds() {
        let items = build(&sources_with(Some(ready_phone()), 0));
        let accel_of = |title: &str| {
            items
                .iter()
                .find(|i| i.title == title)
                .and_then(|i| i.accel)
        };
        assert_eq!(accel_of("Send files"), Some("<control>o"));
        assert_eq!(accel_of("Pair another phone"), Some("<control>n"));
        assert_eq!(accel_of("Connection history"), Some("<control>h"));
        assert_eq!(accel_of("Keyboard shortcuts"), Some("<control>question"));
    }

    #[test]
    fn phone_rows_name_their_phone_and_sound_rows_say_what_they_will_do() {
        let mut phone = ready_phone();
        phone.phone_mic_on = true;
        let items = build(&sources_with(Some(phone.clone()), 1));
        let send = items.iter().find(|i| i.title == "Send files").unwrap();
        assert_eq!(
            send.target,
            Target::PhoneAction(phone.id.clone(), Action::SendFiles)
        );
        let mic = items
            .iter()
            .find(|i| i.title.contains("microphone"))
            .unwrap();
        assert_eq!(mic.title, "Stop using Pixel 9 as microphone");
        assert_eq!(
            mic.target,
            Target::PhoneAction(phone.id.clone(), Action::Sound("phone-mic", false))
        );
        let play = items
            .iter()
            .find(|i| i.title.starts_with("Play Pixel"))
            .unwrap();
        assert_eq!(
            play.target,
            Target::PhoneAction(phone.id, Action::Sound("phone-sound", true))
        );
        // This computer's own rows are not tied to a phone.
        let new_note = items.iter().find(|i| i.title == "New note").unwrap();
        assert_eq!(new_note.target, Target::Action(Action::NewNote));
    }

    #[test]
    fn idle_hides_snippets_and_what_a_note_says() {
        let mut sources = sources_with(Some(ready_phone()), 0);
        sources.snippets = vec![("login".into(), "ssh me@secret".into())];
        sources.notes = vec![NoteEntry {
            id: "n".into(),
            title: "Plan".into(),
            preview: "the password is hunter2".into(),
            body: "Plan\nthe password is hunter2".into(),
            pinned: false,
        }];
        let items = build(&sources);
        let idle = search(&items, "");
        assert!(idle.iter().all(|s| s.group != Group::Snippets));
        let note = idle.iter().find(|s| s.group == Group::Notes).unwrap();
        assert!(items[note.items[0]].private_subtitle);
        // Typed, both are found.
        assert_eq!(count(&search(&items, "login")), 1);
        assert_eq!(count(&search(&items, "hunter2")), 1);
    }

    #[test]
    fn a_huge_text_is_cut_without_reading_all_of_it() {
        let huge = format!("start {}", "word ".repeat(2_000_000));
        let started = std::time::Instant::now();
        let line = one_line(&huge, 90);
        assert!(started.elapsed() < std::time::Duration::from_millis(250));
        assert_eq!(line.chars().count(), 90);
        assert!(line.ends_with('…') && line.starts_with("start word"));
        // Whitespace-heavy input that collapses short is still marked as cut.
        let spaced = format!("a{}b", " ".repeat(1_000));
        assert!(one_line(&spaced, 20).ends_with('…'));
        assert_eq!(one_line("  a   b ", 20), "a b");
    }
}
