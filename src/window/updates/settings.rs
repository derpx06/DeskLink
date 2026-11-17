//! The Updates controls in Settings (Advanced, next to About): Check for
//! updates with its progress and result in the row, Check automatically, the
//! channel, "Skipped x.y.z" with Undo while something is skipped, and one
//! plain line about what a check sends. Controls only: no update appears in
//! any list or page. With no update address in the build, one line says so.
//!
//! Sibling edges: `words`, `coordinator`. Outside: `crate::update`,
//! `DaemonCommand` for the two settings.

use super::*;
use crate::device_links::config::AppSettings;
use crate::device_links::daemon::{DaemonCommand, DaemonHandle};
use crate::update::{Command, Decision, Snapshot, Status};
use crate::window::prefs;
use std::time::Duration;

fn plain_row(title: &str, subtitle: &str) -> adw::ActionRow {
    adw::ActionRow::builder()
        .title(title)
        .subtitle(subtitle)
        .use_markup(false)
        .subtitle_lines(0)
        .build()
}

/// "today, 20:04", or the date for an older check (local time).
fn checked_text(at: u64) -> String {
    let Ok(time) = glib::DateTime::from_unix_local(at as i64) else {
        return String::new();
    };
    let hm = time
        .format("%H:%M")
        .map(|s| s.to_string())
        .unwrap_or_default();
    let today = glib::DateTime::now_local()
        .map(|now| now.format("%Y-%m-%d").ok() == time.format("%Y-%m-%d").ok())
        .unwrap_or(false);
    if today {
        format!("today, {hm}")
    } else {
        let day = time
            .format("%e %b")
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        format!("{day}, {hm}")
    }
}

/// The line under "Check for updates": the last result, or the version.
fn result_text(snapshot: &Snapshot) -> String {
    match &snapshot.last_check {
        Status::Failed { .. } => words::CHECK_FAILED.to_string(),
        _ if snapshot.last_checked == 0 => format!("DeskLink {}", snapshot.installed),
        _ => format!(
            "{} · {}",
            words::UP_TO_DATE,
            words::up_to_date_detail(
                &snapshot.installed.to_string(),
                &checked_text(snapshot.last_checked)
            )
        ),
    }
}

fn check_row(window_of: impl Fn() -> Option<DesklinkWindow> + Clone + 'static) -> adw::ActionRow {
    let snapshot = crate::update::handle().map(|h| h.snapshot());
    let row = plain_row(words::CHECK_ROW, "");
    prefs::with_icon(&row, "desklink-refresh-symbolic");
    if let Some(snapshot) = &snapshot {
        row.set_subtitle(&result_text(snapshot));
    }
    let spinner = gtk::Spinner::builder()
        .visible(false)
        .valign(gtk::Align::Center)
        .build();
    spinner.update_property(&[gtk::accessible::Property::Label(words::CHECKING)]);
    let button = ui_kit::button(ButtonKind::Tonal, words::CHECK_BUTTON, None);
    button.set_valign(gtk::Align::Center);
    row.add_suffix(&spinner);
    row.add_suffix(&button);
    row.set_activatable_widget(Some(&button));
    row.update_property(&[gtk::accessible::Property::Description(words::CHECK_ROW)]);
    let (row_weak, spinner_weak) = (row.downgrade(), spinner.downgrade());
    button.connect_clicked(move |button| {
        let (Some(update), Some(row)) = (crate::update::handle(), row_weak.upgrade()) else {
            return;
        };
        let before = update.snapshot().generation;
        button.set_sensitive(false);
        if let Some(spinner) = spinner_weak.upgrade() {
            spinner.set_visible(true);
            spinner.start();
        }
        row.set_subtitle(words::CHECKING);
        update.send(Command::Check { manual: true });
        let (button, spinner, row, window_of) = (
            button.downgrade(),
            spinner_weak.clone(),
            row.downgrade(),
            window_of.clone(),
        );
        let mut waited = 0u64;
        glib::timeout_add_local(Duration::from_millis(200), move || {
            let (Some(update), Some(row)) = (crate::update::handle(), row.upgrade()) else {
                return glib::ControlFlow::Break;
            };
            let now = update.snapshot();
            waited += 200;
            // The service answers a manual check in well under the fetch
            // budget; past it the row reads as a failed check.
            if now.generation == before && waited < 13_000 {
                return glib::ControlFlow::Continue;
            }
            if let Some(spinner) = spinner.upgrade() {
                spinner.stop();
                spinner.set_visible(false);
            }
            let found = matches!(
                now.manual_decision,
                Decision::Available(_) | Decision::Required(_)
            );
            if now.generation == before {
                row.set_subtitle(words::CHECK_FAILED);
            } else if found && !matches!(now.last_check, Status::Failed { .. }) {
                row.set_subtitle(&format!("DeskLink {}", now.installed));
                if let Some(window) = window_of() {
                    show_requested(&window);
                }
            } else {
                row.set_subtitle(&result_text(&now));
            }
            if let Some(button) = button.upgrade() {
                button.set_sensitive(true);
                button.grab_focus();
            }
            glib::ControlFlow::Break
        });
    });
    row
}

/// Tells the update thread a setting changed, once the daemon has saved it
/// (the setting is read from the mirror the daemon writes), or after 2 s.
fn poke_when_saved(saved: impl Fn(&AppSettings) -> bool + 'static) {
    let mut waited = 0u32;
    glib::timeout_add_local(Duration::from_millis(50), move || {
        waited += 50;
        if saved(&crate::device_links::config::app_settings()) || waited >= 2_000 {
            if let Some(update) = crate::update::handle() {
                update.send(Command::Settings);
            }
            return glib::ControlFlow::Break;
        }
        glib::ControlFlow::Continue
    });
}

fn auto_row(daemon: &DaemonHandle, settings: &AppSettings) -> adw::SwitchRow {
    let row = adw::SwitchRow::builder()
        .title(words::AUTO_ROW)
        .subtitle(if settings.update_check {
            words::AUTO_SUB
        } else {
            words::AUTO_OFF_SUB
        })
        .use_markup(false)
        .active(settings.update_check)
        .build();
    prefs::with_icon(&row, "desklink-history-symbolic");
    let daemon = daemon.clone();
    row.connect_active_notify(move |row| {
        let on = row.is_active();
        daemon.send(DaemonCommand::SetUpdateCheck(on));
        // The title stays; only the line under it says the state.
        row.set_subtitle(if on {
            words::AUTO_SUB
        } else {
            words::AUTO_OFF_SUB
        });
        poke_when_saved(move |settings| settings.update_check == on);
    });
    row
}

fn channel_row(
    daemon: &DaemonHandle,
    settings: &AppSettings,
    installed: crate::update::Version,
) -> adw::ComboRow {
    let row = adw::ComboRow::builder()
        .title(words::CHANNEL_ROW)
        .model(&gtk::StringList::new(&[
            words::CHANNEL_BETA,
            words::CHANNEL_STABLE,
        ]))
        .use_markup(false)
        .build();
    let beta_line = installed.is_beta_line();
    let chosen = crate::update::Channel::chosen(&settings.update_channel, installed);
    prefs::with_icon(&row, "desklink-badge-symbolic");
    row.set_selected(u32::from(chosen == crate::update::Channel::Stable));
    if beta_line {
        // Stable starts at 1.0: nothing to choose until then.
        row.set_subtitle(words::STABLE_LATER);
        row.set_selected(0);
        row.set_sensitive(false);
    } else {
        row.set_subtitle(words::CHANNEL_SUB);
        let daemon = daemon.clone();
        row.connect_selected_notify(move |row| {
            let name = if row.selected() == 1 {
                "stable"
            } else {
                "beta"
            };
            daemon.send(DaemonCommand::SetUpdateChannel(name.to_string()));
            poke_when_saved(move |settings| settings.update_channel == name);
        });
    }
    row
}

fn skipped_row(snapshot: &Snapshot) -> Option<adw::ActionRow> {
    let version = snapshot.skipped?;
    let row = plain_row(&words::skipped_row(&version.to_string()), "");
    row.set_subtitle(words::SKIPPED_SUB);
    prefs::with_icon(&row, "desklink-close-symbolic");
    let undo = ui_kit::button(ButtonKind::Text, words::UNDO, None);
    undo.set_valign(gtk::Align::Center);
    row.add_suffix(&undo);
    let row_weak = row.downgrade();
    undo.connect_clicked(move |_| {
        if let Some(update) = crate::update::handle() {
            update.send(Command::Unskip);
        }
        if let Some(row) = row_weak.upgrade() {
            row.set_visible(false);
        }
    });
    Some(row)
}

/// The rows, in order: Check for updates, Check automatically, the channel,
/// "Skipped" (only while one is), and the line about what a check sends.
/// Without an update address in this build: the one line saying so.
pub(in crate::window) fn updates_rows(
    daemon: &DaemonHandle,
    settings: &AppSettings,
    window_of: impl Fn() -> Option<DesklinkWindow> + Clone + 'static,
) -> Vec<gtk::Widget> {
    let Some(update) = crate::update::handle() else {
        return vec![plain_row(words::GROUP_TITLE, words::NOT_SET_UP).upcast()];
    };
    let snapshot = update.snapshot();
    if matches!(snapshot.status, Status::NotConfigured) {
        return vec![plain_row(words::GROUP_TITLE, words::NOT_SET_UP).upcast()];
    }
    let mut rows: Vec<gtk::Widget> = vec![
        check_row(window_of).upcast(),
        auto_row(daemon, settings).upcast(),
        channel_row(daemon, settings, snapshot.installed).upcast(),
    ];
    if let Some(row) = skipped_row(&snapshot) {
        rows.push(row.upcast());
    }
    let note = plain_row(words::WHAT_A_CHECK_SENDS, words::SHARE_NOTE);
    prefs::with_icon(&note, "desklink-info-symbolic");
    note.add_css_class("dl-update-note");
    rows.push(note.upcast());
    rows
}
