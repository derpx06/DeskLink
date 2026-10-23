//! The phone's calls in the window: a card on the phone's overview, the same
//! card as a bar over every other page, and the ringing and missed-call
//! notifications. Answer, Decline, Silence, Mute and End go to the phone as
//! `desklink.telephony.request`; what it does comes back as its next
//! snapshot.

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::{gio, glib};

use super::{can_send_feature, ellipsized, icon_button, icon_square, named_widgets};
use super::{DesklinkWindow, DeviceView};
use crate::device_links::daemon::{DaemonCommand, DaemonHandle};
use crate::device_links::device::{CallState, CallStatus, PhoneCall};
use crate::device_links::packet::PACKET_TYPE_TELEPHONY_REQUEST;

/// "0:42", "12:05", "1:02:03".
pub(super) fn timer_words(started_at_ms: i64, now_ms: i64) -> String {
    let seconds = (now_ms - started_at_ms).max(0) / 1000;
    let (hours, minutes, seconds) = (seconds / 3600, (seconds / 60) % 60, seconds % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

fn now_ms() -> i64 {
    glib::real_time() / 1000
}

/// The line under the caller: what the call is doing.
pub(super) fn detail_words(phone: &str, status: &CallStatus, now_ms: i64) -> String {
    let call = status.primary();
    let mut words = match call.state {
        CallState::Ringing if call.incoming => format!("Incoming call on {phone}"),
        CallState::Ringing | CallState::Dialing => format!("Calling from {phone}…"),
        CallState::Held => format!("On hold on {phone}"),
        CallState::Talking => match call.started_at {
            Some(started) => format!("On a call · {}", timer_words(started, now_ms)),
            None => "On a call".to_string(),
        },
    };
    // The name is the headline; the number goes here when both came.
    if let (Some(_), Some(number)) = (&call.contact_name, &call.number) {
        words.push_str(&format!(" · {number}"));
    }
    if status.muted && call.state != CallState::Ringing {
        words.push_str(" · muted");
    }
    if status.silenced && call.state == CallState::Ringing {
        words.push_str(" · silenced");
    }
    words
}

/// The buttons a call offers now: (action, label, icon, classes).
pub(crate) fn buttons(
    status: &CallStatus,
) -> Vec<(
    &'static str,
    &'static str,
    &'static str,
    &'static [&'static str],
)> {
    let call = status.primary();
    if call.incoming && call.state == CallState::Ringing {
        let mut buttons = vec![
            (
                "answer",
                "Answer",
                "call-start-symbolic",
                &["call-answer"][..],
            ),
            (
                "decline",
                "Decline",
                "call-stop-symbolic",
                &["destructive-action"][..],
            ),
        ];
        if !status.silenced {
            buttons.push((
                "silence",
                "Silence",
                "audio-volume-muted-symbolic",
                &["soft-button"][..],
            ));
        }
        return buttons;
    }
    vec![
        if status.muted {
            (
                "unmute",
                "Unmute",
                "microphone-sensitivity-muted-symbolic",
                &["soft-button", "toggled"][..],
            )
        } else {
            (
                "mute",
                "Mute",
                "audio-input-microphone-symbolic",
                &["soft-button"][..],
            )
        },
        (
            "end",
            "End",
            "call-stop-symbolic",
            &["destructive-action"][..],
        ),
    ]
}

fn ringing_here(call: &PhoneCall) -> bool {
    call.incoming && call.state == CallState::Ringing
}

/// The call as a card (`compact`: the bar over other pages).
pub(super) fn card(daemon: &DaemonHandle, device: &DeviceView, compact: bool) -> Option<gtk::Box> {
    if !device.feature_transport_ready {
        return None;
    }
    let status = device.call_status.as_ref()?;
    let call = status.primary();
    let ringing = ringing_here(call);
    let card = gtk::Box::builder()
        .spacing(14)
        .css_classes(["call-card"])
        .build();
    if compact {
        card.add_css_class("compact");
    } else {
        // On a narrow page the buttons go under the caller.
        card.set_widget_name("responsive");
    }
    if ringing {
        card.add_css_class("ringing");
    }
    let icon = icon_square(
        "call-start-symbolic",
        if compact { 28 } else { 40 },
        "call-icon",
    );
    // The ring pulses, unless the user turned animations off (GNOME's
    // "Reduce animation" sets this).
    if ringing
        && gtk::Settings::default().is_some_and(|settings| settings.is_gtk_enable_animations())
    {
        icon.add_css_class("pulse");
    }
    card.append(&icon);
    let words = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .hexpand(true)
        .valign(gtk::Align::Center)
        .build();
    let caller = call.caller();
    words.append(&ellipsized(
        &caller,
        &[if compact { "heading" } else { "title-3" }],
    ));
    let detail = ellipsized(&detail_words(&device.name, status, now_ms()), &["text-dim"]);
    if let (CallState::Talking, Some(started)) = (call.state, call.started_at) {
        detail.set_widget_name(&format!("call-timer:{started}"));
    }
    words.append(&detail);
    card.append(&words);
    card.update_property(&[gtk::accessible::Property::Label(&format!(
        "{caller}, {}",
        detail_words(&device.name, status, now_ms())
    ))]);
    let can = can_send_feature(device, PACKET_TYPE_TELEPHONY_REQUEST);
    // On a narrow page the buttons wrap under the caller.
    let actions = super::overview::wrap_row(1, 3);
    actions.set_column_spacing(8);
    actions.set_row_spacing(8);
    if !compact {
        actions.set_widget_name("hero-wrap");
    }
    for (action, label, icon, classes) in buttons(status) {
        // The bar over other pages stays one line at any width: icons only,
        // their words in the tooltip and for screen readers.
        let button = if compact {
            let button = gtk::Button::builder()
                .icon_name(icon)
                .tooltip_text(label)
                .css_classes(
                    classes
                        .iter()
                        .copied()
                        .chain(["circular"])
                        .collect::<Vec<_>>(),
                )
                .build();
            button.update_property(&[gtk::accessible::Property::Label(label)]);
            button
        } else {
            icon_button(icon, label, classes)
        };
        button.set_sensitive(can);
        if !can {
            button.set_tooltip_text(Some("Update DeskLink on the phone to answer here"));
        }
        let (daemon, id) = (daemon.clone(), device.id.clone());
        button.connect_clicked(move |_| {
            daemon.send(DaemonCommand::Telephony(
                id.clone(),
                action.to_string(),
                None,
            ))
        });
        super::overview::add_wrapped(&actions, &button);
    }
    card.append(&actions);
    Some(card)
}

/// The phone with a call to show: one ringing first.
fn calling(devices: &[DeviceView]) -> Option<&DeviceView> {
    let live = || {
        devices
            .iter()
            .filter(|device| device.trusted && device.feature_transport_ready)
            .filter(|device| device.call_status.is_some())
    };
    live()
        .find(|device| {
            device
                .call_status
                .as_ref()
                .is_some_and(|status| status.ringing().is_some())
        })
        .or_else(|| live().next())
}

/// The bar over every page but that phone's own overview (which has the
/// card), redrawn when the call changes; its timer ticks in place.
pub(super) fn update_bar(win: &DesklinkWindow, daemon: &DaemonHandle, devices: &[DeviceView]) {
    let imp = win.imp();
    let on_overview = |device: &DeviceView| {
        imp.page.borrow().as_str() == "home"
            && imp.selected_device_id.borrow().as_deref() == Some(device.id.as_str())
    };
    let shown = calling(devices).filter(|device| !on_overview(device));
    let key = shown
        .map(|device| serde_json::to_string(&(&device.id, &device.call_status)).unwrap_or_default())
        .unwrap_or_default();
    if *imp.call_bar_key.borrow() != key {
        imp.call_bar_key.replace(key);
        match shown.and_then(|device| card(daemon, device, true)) {
            Some(bar) => {
                bar.set_margin_start(12);
                bar.set_margin_end(12);
                bar.set_margin_top(6);
                bar.set_margin_bottom(6);
                imp.call_bar.set_child(Some(&bar));
                imp.call_bar.set_reveal_child(true);
            }
            None => imp.call_bar.set_reveal_child(false),
        }
    }
    tick(&imp.call_bar.clone().upcast());
    if let Some((_, body)) = imp.bodies.borrow().get("home").cloned() {
        tick(&body.upcast());
    }
}

/// Moves every call timer under `root` to now.
fn tick(root: &gtk::Widget) {
    for widget in named_widgets(root, "call-timer:") {
        let Some(label) = widget.downcast_ref::<gtk::Label>() else {
            continue;
        };
        let Ok(started) = widget
            .widget_name()
            .trim_start_matches("call-timer:")
            .parse::<i64>()
        else {
            continue;
        };
        let text = label.label();
        // "On a call · 1:05 · …": only the time moves.
        let mut parts: Vec<String> = text.split(" · ").map(str::to_string).collect();
        if parts.len() >= 2 {
            parts[1] = timer_words(started, now_ms());
            let updated = parts.join(" · ");
            if updated != text {
                label.set_label(&updated);
            }
        }
    }
}

/// A notification for each call ringing here — unless the top-bar
/// extension shows its own banner — taken back once it stops ringing.
pub(super) fn notify(win: &DesklinkWindow, devices: &[DeviceView]) {
    let Some(app) = win.application() else {
        return;
    };
    let imp = win.imp();
    let ringing: Vec<(&DeviceView, &PhoneCall)> = devices
        .iter()
        .filter(|device| device.trusted && device.feature_transport_ready)
        .filter_map(|device| {
            let call = device.call_status.as_ref()?.ringing()?;
            Some((device, call))
        })
        .collect();
    // Keyed by what it says too: the caller's name often arrives a moment
    // after the ring, and the notification must then say who it is.
    let key = |device: &DeviceView, call: &PhoneCall| {
        format!(
            "{}:{}:{}:{}",
            device.id,
            call.call_id,
            call.caller(),
            call.number.as_deref().unwrap_or_default()
        )
    };
    let keys: std::collections::HashSet<String> = ringing
        .iter()
        .map(|(device, call)| key(device, call))
        .collect();
    let ended: Vec<String> = imp
        .notified_calls
        .borrow()
        .iter()
        .filter(|key| !keys.contains(*key))
        .cloned()
        .collect();
    for key in ended {
        imp.notified_calls.borrow_mut().remove(&key);
        if let Some((device, _)) = key.split_once(':') {
            app.withdraw_notification(&format!("call-{device}"));
        }
    }
    let banner_elsewhere = crate::shell_dbus::client_handles("calls");
    for (device, call) in ringing {
        if !imp.notified_calls.borrow_mut().insert(key(device, call)) || banner_elsewhere {
            continue;
        }
        let notification = gio::Notification::new(&call.caller());
        let mut body = format!("Incoming call on {}", device.name);
        if let (Some(_), Some(number)) = (&call.contact_name, &call.number) {
            body.push_str(&format!(" · {number}"));
        }
        notification.set_body(Some(&body));
        notification.set_priority(gio::NotificationPriority::Urgent);
        notification.set_icon(&gio::ThemedIcon::new("call-start-symbolic"));
        if can_send_feature(device, PACKET_TYPE_TELEPHONY_REQUEST) {
            for (action, label) in [
                ("answer", "Answer"),
                ("decline", "Decline"),
                ("silence", "Silence"),
            ] {
                notification.add_button_with_target_value(
                    label,
                    "app.call",
                    Some(&(device.id.as_str(), action, "").to_variant()),
                );
            }
        }
        app.send_notification(Some(&format!("call-{}", device.id)), &notification);
    }
}

/// "Missed call from Alice", with Call Back when the number came.
pub(super) fn missed(
    win: &DesklinkWindow,
    device_id: &str,
    device_name: &str,
    caller: Option<String>,
    number: Option<String>,
) {
    let Some(app) = win.application() else {
        return;
    };
    let title = match caller.as_ref().or(number.as_ref()) {
        Some(who) => format!("Missed call from {who}"),
        None => "Missed call".to_string(),
    };
    let notification = gio::Notification::new(&title);
    let mut body = format!("On {device_name}");
    if let (Some(_), Some(number)) = (&caller, &number) {
        body.push_str(&format!(" · {number}"));
    }
    notification.set_body(Some(&body));
    notification.set_icon(&gio::ThemedIcon::new("call-missed-symbolic"));
    if let Some(number) = &number {
        notification.add_button_with_target_value(
            "Call Back",
            "app.call",
            Some(&(device_id, "dial", number.as_str()).to_variant()),
        );
    }
    app.send_notification(
        Some(&format!(
            "missed-call-{device_id}-{}",
            glib::monotonic_time()
        )),
        &notification,
    );
}

/// Asks before calling `number` from a link (`tel:`), `desklink --dial` or
/// a contact: a web page can open `tel:` links without the user meaning to
/// call. With `phone`, that phone places the call; else the first that can.
pub(crate) fn confirm_dial(win: &DesklinkWindow, number: &str, phone: Option<&str>) {
    let Some(daemon) = win.imp().daemon.borrow().clone() else {
        return;
    };
    let Some(number) = crate::device_links::calls::dial_number(number) else {
        win.show_feature_toast("That is not a phone number");
        return;
    };
    let phone = daemon
        .devices()
        .into_iter()
        .filter(|device| device.trusted && device.feature_transport_ready)
        .filter(|device| phone.is_none_or(|id| id == device.id))
        .find(|device| can_send_feature(device, PACKET_TYPE_TELEPHONY_REQUEST));
    let Some(phone) = phone else {
        let dialog = adw::AlertDialog::new(
            Some("No Phone to Call With"),
            Some("Connect a phone with DeskLink to make calls from this computer."),
        );
        dialog.add_response("close", "_Close");
        dialog.present(Some(win));
        return;
    };
    let dialog = adw::AlertDialog::new(
        Some(&format!("Call {number}?")),
        Some(&format!("{} places the call.", phone.name)),
    );
    dialog.add_response("cancel", "_Cancel");
    dialog.add_response("call", "_Call");
    dialog.set_response_appearance("call", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("call"));
    dialog.set_close_response("cancel");
    dialog.connect_response(Some("call"), move |_, _| {
        daemon.send(DaemonCommand::Telephony(
            phone.id.clone(),
            "dial".to_string(),
            Some(number.clone()),
        ));
    });
    dialog.present(Some(win));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(state: CallState, incoming: bool, muted: bool) -> CallStatus {
        CallStatus {
            calls: vec![PhoneCall {
                call_id: "1".into(),
                state,
                contact_name: Some("Alice".into()),
                number: Some("+1 555 0100".into()),
                started_at: Some(1_000),
                incoming,
            }],
            muted,
            silenced: false,
        }
    }

    #[test]
    fn the_timer_reads_minutes_then_hours() {
        assert_eq!(timer_words(1_000, 1_000), "0:00");
        assert_eq!(timer_words(0, 65_000), "1:05");
        assert_eq!(timer_words(0, 3_723_000), "1:02:03");
        assert_eq!(
            timer_words(5_000, 1_000),
            "0:00",
            "a clock skew never goes negative"
        );
    }

    #[test]
    fn a_ringing_call_offers_answer_and_a_live_one_mute_and_end() {
        let actions = |status: &CallStatus| -> Vec<&str> {
            buttons(status)
                .into_iter()
                .map(|(action, ..)| action)
                .collect()
        };
        assert_eq!(
            actions(&status(CallState::Ringing, true, false)),
            ["answer", "decline", "silence"]
        );
        assert_eq!(
            actions(&status(CallState::Talking, true, false)),
            ["mute", "end"]
        );
        assert_eq!(
            actions(&status(CallState::Talking, true, true)),
            ["unmute", "end"]
        );
        assert_eq!(
            actions(&status(CallState::Ringing, false, false)),
            ["mute", "end"],
            "an outgoing call ringing the other end is not answered here"
        );
        let mut silenced = status(CallState::Ringing, true, false);
        silenced.silenced = true;
        assert_eq!(actions(&silenced), ["answer", "decline"]);
    }

    #[test]
    fn the_detail_line_says_what_the_call_is_doing() {
        assert_eq!(
            detail_words("Pixel", &status(CallState::Ringing, true, false), 0),
            "Incoming call on Pixel · +1 555 0100"
        );
        assert_eq!(
            detail_words("Pixel", &status(CallState::Talking, true, true), 66_000),
            "On a call · 1:05 · +1 555 0100 · muted"
        );
        assert_eq!(
            detail_words("Pixel", &status(CallState::Dialing, false, false), 0),
            "Calling from Pixel… · +1 555 0100"
        );
    }
}
