//! The `Shell1` properties as GLib variants: devices, notifications,
//! activities, transfers and snippets.
//!
//! Sibling edges: `model`. Outside: `activity`, `notes`, `transfer_log`,
//! `window::notifications`.

use super::*;

impl ShellDevice {
    fn to_dict(&self) -> Variant {
        let dict = glib::VariantDict::new(None);
        dict.insert("id", &self.id);
        dict.insert("name", &self.name);
        dict.insert("type", &self.kind);
        dict.insert("paired", self.paired);
        dict.insert("reachable", self.reachable);
        dict.insert("ready", self.ready);
        dict.insert("quiet", self.quiet);
        dict.insert("battery", self.battery);
        dict.insert("charging", self.charging);
        dict.insert("path", &self.path);
        dict.insert("screen", self.screen);
        dict.insert("terminals", self.terminals);
        dict.insert("lastSeen", self.last_seen);
        if let Some(call) = &self.call {
            let entry = glib::VariantDict::new(None);
            entry.insert("id", &call.id);
            entry.insert("state", call.state);
            entry.insert("incoming", call.incoming);
            entry.insert("caller", &call.caller);
            entry.insert("number", &call.number);
            entry.insert("startedAt", call.started_at);
            entry.insert("muted", call.muted);
            entry.insert("silenced", call.silenced);
            dict.insert_value("call", &entry.end());
        }
        if let Some(media) = &self.media {
            let entry = glib::VariantDict::new(None);
            entry.insert("title", &media.title);
            entry.insert("artist", &media.artist);
            entry.insert("playing", media.playing);
            entry.insert("positionMs", media.position_ms);
            entry.insert("lengthMs", media.length_ms);
            entry.insert("seekReason", &media.seek_reason);
            dict.insert_value("media", &entry.end());
        }
        if let Some(bucket) = &self.bucket {
            let entry = glib::VariantDict::new(None);
            entry.insert("line", &bucket.line);
            entry.insert("detail", &bucket.detail);
            entry.insert("fraction", f64::from(bucket.permille) / 1000.0);
            dict.insert_value("bucket", &entry.end());
        }
        dict.insert("liveSince", self.live_since);
        let switches = glib::VariantDict::new(None);
        for (key, on) in &self.settings {
            switches.insert(key, *on);
        }
        dict.insert_value("settings", &switches.end());
        let sound = glib::VariantDict::new(None);
        sound.insert("deskSound", self.sound[0]);
        sound.insert("phoneMic", self.sound[1]);
        sound.insert("phoneSound", self.sound[2]);
        dict.insert_value("sound", &sound.end());
        dict.insert("rttMs", self.rtt_ms);
        dict.insert("unread", self.unread);
        dict.insert("textCapture", self.text_capture);
        let transfers = self.transfers.iter().map(|transfer| {
            let entry = glib::VariantDict::new(None);
            entry.insert("name", &transfer.name);
            entry.insert("incoming", transfer.incoming);
            entry.insert("fraction", f64::from(transfer.permille) / 1000.0);
            entry.insert("paused", transfer.paused);
            entry.insert("lane", &transfer.lane);
            entry.end()
        });
        dict.insert_value(
            "transfers",
            &Variant::array_from_iter_with_type(glib::VariantTy::VARDICT, transfers),
        );
        dict.insert("actions", &self.actions);
        dict.end()
    }
}

/// The phones' notifications, for the extension's message tray: only phones
/// whose notifications are shown here. `silent` ones were resent on a
/// reconnect and never banner; `ready` false means they are stale (no
/// actions).
pub(super) fn notifications_variant(devices: &[DeviceView], settings: &AppSettings) -> Variant {
    let dicts = devices
        .iter()
        .filter(|device| device.trusted && settings.shows_notifications(&device.id))
        .flat_map(|device| {
            device.notifications.iter().map(move |notification| {
                let dict = glib::VariantDict::new(None);
                dict.insert("device", &device.id);
                dict.insert("deviceName", &device.name);
                dict.insert("id", &notification.id);
                dict.insert("app", &notification.app_name);
                dict.insert(
                    "icon",
                    crate::window::notifications::app_icon(&notification.app_name),
                );
                dict.insert("title", &notification.title);
                // One paragraph: the tray and the reply dialog cap lines
                // per paragraph, or not at all.
                dict.insert(
                    "body",
                    crate::window::notifications::one_paragraph(if notification.text.is_empty() {
                        &notification.ticker
                    } else {
                        &notification.text
                    }),
                );
                dict.insert("actions", &notification.actions);
                dict.insert("replyable", notification.request_reply_id.is_some());
                dict.insert("dismissable", notification.is_clearable);
                dict.insert("silent", notification.silent);
                dict.insert("ready", device.feature_transport_ready);
                dict.end()
            })
        });
    Variant::array_from_iter_with_type(glib::VariantTy::VARDICT, dicts)
}

/// The phones' ongoing items (W13) that show now, for the top-bar chip. `base`
/// is on THIS computer's clock (milliseconds since the epoch), -1 for none,
/// so the extension needs no offset of its own.
pub(super) fn activities_variant(
    devices: &[DeviceView],
    settings: &AppSettings,
    now_ms: i64,
) -> Variant {
    let dicts = devices
        .iter()
        .filter(|device| device.trusted)
        .flat_map(|device| {
            let offset = device.activity.as_ref().map_or(0, |a| a.clock_offset_ms);
            crate::device_links::activity::shown(
                device.activity.as_ref(),
                device.feature_transport_ready,
                settings.shows_notifications(&device.id),
                now_ms,
            )
            .into_iter()
            .map(move |item| {
                let dict = glib::VariantDict::new(None);
                dict.insert("device", &device.id);
                dict.insert("deviceName", &device.name);
                dict.insert("id", &item.id);
                dict.insert("app", &item.app_label);
                dict.insert("title", &item.title);
                dict.insert("kind", &item.kind);
                dict.insert("progress", i64::from(item.progress));
                dict.insert(
                    "base",
                    if item.chronometer_base > 0 {
                        item.chronometer_base.saturating_add(offset)
                    } else {
                        -1
                    },
                );
                dict.insert("countdown", item.countdown);
                dict.end()
            })
            .collect::<Vec<_>>()
        });
    Variant::array_from_iter_with_type(glib::VariantTy::VARDICT, dicts)
}

/// The newest files sent or received; `id` is the path, which only
/// `OpenTransfer` reads back (and only for a file still on this list).
pub(super) fn transfers_variant(
    files: &[crate::device_links::transfer_log::LoggedFile],
) -> Variant {
    let dicts = files.iter().take(RECENT_TRANSFERS).map(|file| {
        let dict = glib::VariantDict::new(None);
        dict.insert("id", file.path.to_string_lossy().as_ref());
        dict.insert("name", &file.name);
        dict.insert("deviceName", &file.device_name);
        dict.insert("incoming", file.incoming);
        dict.insert("time", file.ts);
        dict.end()
    });
    Variant::array_from_iter_with_type(glib::VariantTy::VARDICT, dicts)
}

/// The snippets the menu shows (F6), newest first.
pub(super) fn snippets_variant() -> Variant {
    let dicts = crate::device_links::notes::snippets()
        .into_iter()
        .take(RECENT_SNIPPETS)
        .map(|snippet| {
            let dict = glib::VariantDict::new(None);
            dict.insert("id", &snippet.id);
            dict.insert("label", snippet.label());
            dict.insert("text", &snippet.text);
            dict.end()
        });
    Variant::array_from_iter_with_type(glib::VariantTy::VARDICT, dicts)
}

/// How many snippets the top-bar menu gets.
const RECENT_SNIPPETS: usize = 8;

pub(super) fn devices_variant(devices: &[ShellDevice]) -> Variant {
    Variant::array_from_iter_with_type(
        glib::VariantTy::VARDICT,
        devices.iter().map(ShellDevice::to_dict),
    )
}
