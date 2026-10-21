//! Connection history: every connection and session from a phone, grouped
//! by day, newest first. Shown as the Connection History page, and as a
//! dialog when no phone is paired yet.

use adw::prelude::*;
use gtk::glib;

use super::DesklinkWindow;
use crate::device_links::device::DeviceView;

/// How many entries are listed before "Show Older".
const PAGE: usize = 200;

thread_local! {
    /// How many the History page lists now; "Show Older" adds a page.
    static LIMIT: std::cell::Cell<usize> = const { std::cell::Cell::new(PAGE) };
}

/// "Show Older": the next [`PAGE`] entries.
pub(super) fn show_more() {
    LIMIT.with(|limit| limit.set(limit.get() + PAGE));
}

pub(super) fn update(win: &DesklinkWindow, device: &DeviceView) {
    let entries = crate::device_links::connection_log::recent();
    let minute = super::clock_time(glib::DateTime::now_local().ok());
    let limit = LIMIT.with(std::cell::Cell::get);
    let key = format!(
        "{}\n{}\n{:?}\n{minute}\n{limit}",
        device.id,
        entries.len(),
        entries.first().map(|entry| entry.ts)
    );
    if !win.page_changed("history", key) {
        return;
    }
    let types = win
        .daemon()
        .map(|daemon| {
            win.known_devices(&daemon)
                .into_iter()
                .map(|known| (known.id, known.device_type))
                .collect()
        })
        .unwrap_or_default();
    let page = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(4)
        .margin_top(8)
        .margin_start(16)
        .margin_end(16)
        .build();
    let note = super::text(
        &format!(
            "Every connection and session from {}, kept for 30 days on this computer.",
            device.name
        ),
        &["text-dim", "body-small"],
    );
    note.set_wrap(true);
    note.set_margin_start(12);
    note.set_margin_end(12);
    page.append(&note);
    page.append(&history_content(&types, Some(&device.id), limit));
    win.replace_body(
        "history",
        &adw::Clamp::builder().maximum_size(760).child(&page).build(),
    );
}

/// Connection history grouped by day, newest first, with a time for each:
/// one device's when `only` names it, the newest `limit`, then "Show
/// Older". `types` maps device ids to their type, for each row's glyph.
pub(crate) fn history_content(
    types: &std::collections::HashMap<String, String>,
    only: Option<&str>,
    limit: usize,
) -> gtk::Widget {
    use crate::device_links::connection_log;
    let entries: Vec<_> = connection_log::recent()
        .into_iter()
        .filter(|entry| only.is_none_or(|id| entry.device_id == id))
        .collect();
    if entries.is_empty() {
        return adw::StatusPage::builder()
            .icon_name("document-open-recent-symbolic")
            .title("No Activity Yet")
            .description("Connections from your phones are listed here for 30 days.")
            .vexpand(true)
            .build()
            .upcast();
    }
    let now = glib::DateTime::now_local().ok();
    let page = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(24)
        .margin_top(12)
        .margin_bottom(24)
        .margin_start(12)
        .margin_end(12)
        .build();
    let mut group: Option<(String, adw::PreferencesGroup)> = None;
    let older = entries.len().saturating_sub(limit);
    for entry in entries.into_iter().take(limit) {
        let Some(when) = glib::DateTime::from_unix_local(entry.ts as i64).ok() else {
            continue;
        };
        let day = now
            .as_ref()
            .map(|now| day_label(now, &when))
            .unwrap_or_default();
        if group.as_ref().is_none_or(|(shown, _)| *shown != day) {
            let next = adw::PreferencesGroup::builder().title(&day).build();
            page.append(&next);
            group = Some((day, next));
        }
        let device = if entry.device_name.is_empty() {
            entry.device_id.clone()
        } else {
            entry.device_name.clone()
        };
        // Title: what happened. Subtitle: which device, and how it was connected.
        let mut subtitle = device;
        if let Some(path) = entry
            .path
            .as_deref()
            .and_then(crate::device_links::device::connection_path_label)
        {
            subtitle = format!("{subtitle} · {path}");
        }
        if let Some(detail) = &entry.detail {
            subtitle = format!("{subtitle} · {detail}");
        }
        let row = adw::ActionRow::builder()
            .title(entry.event.label())
            .subtitle(subtitle)
            .use_markup(false)
            .build();
        // Colour is for failures only.
        let failed = matches!(
            entry.event,
            crate::device_links::connection_log::ConnectionEvent::PasswordRejected
                | crate::device_links::connection_log::ConnectionEvent::UnlockRefused
        );

        let icon = super::icon_square(event_icon(entry.event), 32, "row-icon");
        if failed {
            icon.add_css_class("error");
        }
        row.add_prefix(&icon);
        if let Some(kind) = types.get(&entry.device_id).filter(|_| only.is_none()) {
            row.add_suffix(
                &gtk::Image::builder()
                    .icon_name(super::device_type_icon(kind))
                    .tooltip_text(super::device_type_label(kind))
                    .css_classes(["dim-label"])
                    .build(),
            );
        }
        let time = now
            .as_ref()
            .map(|now| time_label(now, &when))
            .unwrap_or_default();
        row.add_suffix(
            &gtk::Label::builder()
                .label(time)
                .tooltip_text(
                    when.format("%e %B %Y, %H:%M")
                        .map(|text| text.trim().to_string())
                        .unwrap_or_default(),
                )
                .css_classes(["text-faint", "caption", "numeric"])
                .build(),
        );
        if let Some((_, group)) = &group {
            group.add(&row);
        }
    }
    if older > 0 {
        page.append(
            &gtk::Button::builder()
                .label(format!("Show Older ({older} more)"))
                .halign(gtk::Align::Center)
                .action_name("win.history-more")
                .css_classes(["soft-button"])
                .build(),
        );
    }
    page.upcast()
}

/// The icon for a history event; a rejected password is the one in red.
fn event_icon(event: crate::device_links::connection_log::ConnectionEvent) -> &'static str {
    use crate::device_links::connection_log::ConnectionEvent::*;
    match event {
        Connected => "network-transmit-receive-symbolic",
        Disconnected => "network-offline-symbolic",
        ScreenViewStarted | ScreenControlStarted => "screen-shared-symbolic",
        ScreenSessionEnded => "video-display-symbolic",
        TerminalAttached | TerminalDetached => "utilities-terminal-symbolic",
        PasswordRejected => "dialog-warning-symbolic",
        SessionsEnded => "process-stop-symbolic",
        Route => "network-workgroup-symbolic",
        SettingsChanged => "emblem-system-symbolic",
        UnlockRefused => "dialog-warning-symbolic",
        UnlockedByPhone => "changes-allow-symbolic",
    }
}

/// "Today", "Yesterday", a weekday within the week, else the date.
fn day_label(now: &glib::DateTime, when: &glib::DateTime) -> String {
    let midnight = |time: &glib::DateTime| {
        let (year, month, day) = time.ymd();
        glib::DateTime::from_local(year, month, day, 0, 0, 0.0).ok()
    };
    let days = match (midnight(now), midnight(when)) {
        // Rounded, so a day with a daylight-saving change still counts as one.
        (Some(today), Some(then)) => (today.difference(&then).as_seconds() + 43_200) / 86_400,
        _ => i64::MAX,
    };
    let format = match days {
        0 => return "Today".to_string(),
        1 => return "Yesterday".to_string(),
        2..=6 => "%A",
        _ => "%e %B",
    };
    when.format(format)
        .map(|text| text.trim().to_string())
        .unwrap_or_default()
}

/// "Just now", "12 min ago" and "3 h ago" for the last few hours, else the
/// clock time.
pub(super) fn time_label(now: &glib::DateTime, when: &glib::DateTime) -> String {
    let seconds = now.difference(when).as_seconds();
    match seconds {
        ..60 => "Just now".to_string(),
        60..3600 => format!("{} min ago", seconds / 60),
        3600..14400 => format!("{} h ago", seconds / 3600),
        _ => when
            .format("%H:%M")
            .map(|text| text.to_string())
            .unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(year: i32, month: i32, day: i32, hour: i32, minute: i32) -> glib::DateTime {
        glib::DateTime::from_local(year, month, day, hour, minute, 0.0).unwrap()
    }

    #[test]
    fn history_days_read_as_today_yesterday_then_dates() {
        let now = at(2026, 10, 20, 9, 0);
        assert_eq!(day_label(&now, &at(2026, 10, 20, 0, 5)), "Today");
        assert_eq!(day_label(&now, &at(2026, 10, 19, 23, 50)), "Yesterday");
        assert_eq!(day_label(&now, &at(2026, 10, 16, 12, 0)), "Friday");
        assert_eq!(day_label(&now, &at(2026, 10, 1, 12, 0)), "1 October");
        // Across a new year, yesterday is still yesterday.
        assert_eq!(
            day_label(&at(2027, 1, 1, 9, 0), &at(2026, 12, 31, 22, 0)),
            "Yesterday"
        );
    }

    #[test]
    fn recent_history_times_are_relative_and_older_ones_are_clock_times() {
        let now = at(2026, 10, 20, 15, 0);
        assert_eq!(time_label(&now, &at(2026, 10, 20, 14, 59)), "1 min ago");
        assert_eq!(time_label(&now, &now), "Just now");
        assert_eq!(time_label(&now, &at(2026, 10, 20, 12, 30)), "2 h ago");
        assert_eq!(time_label(&now, &at(2026, 10, 20, 8, 5)), "08:05");
    }
}
