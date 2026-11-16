//! What a transfer card says, with no widgets (glib only for dates): the
//! subtitle, the fraction, the row state, the file class and glyph, the
//! filters, empty-page words, day groups and the rate text. Unit-tested.
//!
//! Sibling edges: none.

use super::*;

/// "Sending to Pixel · 4.2 MB of 40 MB".
pub(super) fn subtitle(row: &TransferRow, device: &str) -> String {
    if row.preparing {
        return format!("Preparing to send to {device}…");
    }
    if let Some(note) = crate::device_links::webrtc::transfer_board::note(&row.id) {
        return note;
    }
    if row.paused {
        return format!("Paused — resumes when {device} reconnects");
    }
    let direction = if row.incoming {
        format!("Receiving from {device}")
    } else {
        format!("Sending to {device}")
    };
    // The lane comes right after the direction: the line wraps on the card,
    // and it used to be the tail that an ellipsis cut off.
    let lane = crate::device_links::webrtc::transfer_board::lane_of(&row.id)
        .map(|lane| format!(" · {lane}"))
        .unwrap_or_default();
    format!(
        "{direction}{lane} · {} of {}",
        crate::bucket_progress::size_words(row.done),
        crate::bucket_progress::size_words(row.total)
    )
}

/// A rate and a time left are shown only while bytes are meant to be moving: a
/// row that waits (its note says why, or its device is away) keeps no stale
/// "3 MB/s · 5 s left" from before it stopped.
pub(super) fn shows_rate(row: &TransferRow, held: bool) -> bool {
    !row.preparing && !row.paused && !held
}

/// How far along, 0..=1; `None` while there is nothing to measure against.
pub(in crate::window) fn fraction(row: &TransferRow) -> Option<f64> {
    if row.preparing || row.total == 0 {
        return None;
    }
    Some((row.done as f64 / row.total as f64).clamp(0.0, 1.0))
}

/// What a moving row is doing, for its pill. `held` is a note saying why it
/// waits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RowState {
    /// A file being copied and hashed.
    Preparing,
    /// Bytes are meant to be moving.
    Moving,
    /// The connection dropped; it resumes by itself.
    Paused,
    /// Waiting for something else (a slot, space): the note says what.
    Waiting,
}

pub(super) fn row_state(row: &TransferRow, held: bool) -> RowState {
    if row.preparing {
        RowState::Preparing
    } else if row.paused {
        RowState::Paused
    } else if held {
        RowState::Waiting
    } else {
        RowState::Moving
    }
}

impl RowState {
    /// The pill's state and word; a moving row has none (its percent says it).
    pub(super) fn pill(self) -> Option<(Status, &'static str)> {
        match self {
            Self::Moving => None,
            Self::Preparing => Some((Status::Idle, "Preparing")),
            Self::Paused => Some((Status::Idle, "Paused")),
            Self::Waiting => Some((Status::Warn, "Waiting")),
        }
    }
}

/// The kinds of file the chips tell apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Class {
    Image,
    Video,
    Audio,
    Pdf,
    Document,
    Archive,
    Other,
}

/// What a file is, from its name.
fn class_of(name: &str) -> Class {
    use crate::thumbnails::{kind_of, FileKind};
    match kind_of(std::path::Path::new(name)) {
        FileKind::Image => return Class::Image,
        FileKind::Video => return Class::Video,
        FileKind::Pdf => return Class::Pdf,
        FileKind::Other => {}
    }
    let extension = std::path::Path::new(name)
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_lowercase)
        .unwrap_or_default();
    match extension.as_str() {
        "zip" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "zst" | "7z" | "rar" => Class::Archive,
        "doc" | "docx" | "odt" | "rtf" | "txt" | "md" | "xls" | "xlsx" | "ods" | "csv" | "ppt"
        | "pptx" | "odp" | "epub" => Class::Document,
        "mp3" | "flac" | "ogg" | "opus" | "wav" | "m4a" | "aac" => Class::Audio,
        _ => Class::Other,
    }
}

/// The kit glyph for a file of this name.
pub(in crate::window) fn glyph_for(name: &str) -> &'static str {
    match class_of(name) {
        Class::Image => "desklink-image-symbolic",
        Class::Video => "desklink-movie-symbolic",
        Class::Audio => "desklink-music-note-symbolic",
        Class::Pdf => "desklink-picture-as-pdf-symbolic",
        Class::Archive => "desklink-folder-zip-symbolic",
        Class::Document | Class::Other => "desklink-description-symbolic",
    }
}

/// Which files the history shows: one choice, as the chips.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::window) enum Filter {
    #[default]
    All,
    Received,
    Sent,
    Images,
    Documents,
    Archives,
}

impl Filter {
    pub(super) const CHIPS: [Filter; 6] = [
        Self::All,
        Self::Received,
        Self::Sent,
        Self::Images,
        Self::Documents,
        Self::Archives,
    ];

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Received => "Received",
            Self::Sent => "Sent",
            Self::Images => "Images",
            Self::Documents => "Documents",
            Self::Archives => "Archives",
        }
    }

    pub(super) fn matches(self, entry: &LoggedFile) -> bool {
        match self {
            Self::All => true,
            Self::Received => entry.incoming,
            Self::Sent => !entry.incoming,
            Self::Images => class_of(&entry.name) == Class::Image,
            // A PDF is a document.
            Self::Documents => matches!(class_of(&entry.name), Class::Document | Class::Pdf),
            Self::Archives => class_of(&entry.name) == Class::Archive,
        }
    }
}

/// The empty state's title and words: nothing at all yet, or nothing under
/// this chip.
pub(super) fn empty_words(filter: Filter, any: bool, device: &str) -> (String, String) {
    if filter == Filter::All || !any {
        (
            "No files yet".to_string(),
            format!(
                "Files you send to or receive from {} show up here for 30 days. Drop files \
                 on this page, or press Send files.",
                // The name never breaks across lines ("Pixel" / "9").
                device.replace(' ', "\u{a0}")
            ),
        )
    } else {
        let none = match filter {
            Filter::Received => "No received files",
            Filter::Sent => "No sent files",
            Filter::Images => "No images",
            Filter::Documents => "No documents",
            Filter::Archives => "No archives",
            Filter::All => "No files yet",
        };
        (
            none.to_string(),
            "Nothing matches this filter in the last 30 days. Choose All to see everything."
                .to_string(),
        )
    }
}

/// Files in the order given, grouped under "Today", "Yesterday", a weekday
/// or a date; a day that comes again later in the list starts a new group.
pub(super) fn day_groups<T>(
    items: impl IntoIterator<Item = (u64, T)>,
    now: &glib::DateTime,
) -> Vec<(String, Vec<T>)> {
    let mut groups: Vec<(String, Vec<T>)> = Vec::new();
    for (ts, item) in items {
        let day = glib::DateTime::from_unix_local(ts as i64)
            .ok()
            .map(|when| crate::window::history::day_label(now, &when))
            .unwrap_or_default();
        match groups.last_mut() {
            Some((last, members)) if *last == day => members.push(item),
            _ => groups.push((day, vec![item])),
        }
    }
    groups
}

/// "3.1 MB/s · 12 s left"; empty until there is a rate to speak of.
pub(super) fn rate_text(bytes_per_second: f64, remaining: u64) -> String {
    if bytes_per_second < 1.0 {
        return String::new();
    }
    let speed = format!(
        "{}/s",
        crate::bucket_progress::size_words(bytes_per_second as u64)
    );
    let seconds = (remaining as f64 / bytes_per_second).ceil() as u64;
    let left = match seconds {
        0 => return speed,
        1..=59 => format!("{seconds} s left"),
        60..=3599 => format!("{} min left", seconds.div_ceil(60)),
        _ => format!("{} h left", seconds.div_ceil(3600)),
    };
    format!("{speed} · {left}")
}
