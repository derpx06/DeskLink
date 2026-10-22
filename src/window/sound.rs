//! Sound both ways in the window (F1 A2/A5): the overview's Sound and Mic
//! round actions, and coral in-use bars, with Stop, while this computer's
//! sound or microphone goes to the phone or the phone's comes here.

use adw::prelude::*;

use super::{ellipsized, icon_square, text, DeviceView};
use crate::device_links::daemon::{DaemonCommand, DaemonHandle};

fn send(daemon: &DaemonHandle, device: &DeviceView, what: &str, on: bool) {
    daemon.send(DaemonCommand::Sound(device.id.clone(), what.into(), on));
}

/// Whether `what` is on for this phone (asked or flowing).
pub(super) fn is_on(device: &DeviceView, what: &str) -> bool {
    let Some(sound) = device.sound.as_ref() else {
        return false;
    };
    match what {
        "desk-sound" => sound.sending.is_some(),
        "phone-mic" => sound.hearing.as_deref() == Some("mic"),
        _ => sound.hearing.as_deref() == Some("system"),
    }
}

/// A button that turns `what` on or off, its words following its state.
pub(super) fn toggle(
    daemon: &DaemonHandle,
    device: &DeviceView,
    what: &'static str,
    button: &gtk::Button,
) {
    let (daemon, view) = (daemon.clone(), device.clone());
    button.connect_clicked(move |_| send(&daemon, &view, what, !is_on(&view, what)));
}

/// The overview's Sound action: a popover with "Play this computer's sound
/// on <phone>" and "Use <phone> as microphone" switches.
pub(super) fn attach_menu(daemon: &DaemonHandle, device: &DeviceView, button: &gtk::Button) {
    let list = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(10)
        .margin_top(10)
        .margin_bottom(10)
        .margin_start(12)
        .margin_end(12)
        .build();
    list.append(&text(&format!("Sound with {}", device.name), &["heading"]));
    for (what, words) in [
        ("desk-sound", "Play this computer's sound there"),
        ("phone-mic", "Use it as this computer's microphone"),
        ("phone-sound", "Play its sound here"),
    ] {
        let row = gtk::Box::builder().spacing(16).build();
        let label = text(words, &[]);
        label.set_hexpand(true);
        label.set_xalign(0.0);
        let switch = gtk::Switch::builder()
            .active(is_on(device, what))
            .valign(gtk::Align::Center)
            .build();
        switch.update_property(&[gtk::accessible::Property::Label(words)]);
        let (daemon, view) = (daemon.clone(), device.clone());
        switch.connect_state_set(move |_, on| {
            send(&daemon, &view, what, on);
            gtk::glib::Propagation::Proceed
        });
        row.append(&label);
        row.append(&switch);
        list.append(&row);
    }
    let popover = gtk::Popover::builder().child(&list).build();
    popover.set_parent(button);
    button.connect_destroy({
        let popover = popover.clone();
        move |_| popover.unparent()
    });
    button.connect_clicked(move |_| popover.popup());
}

fn bar(
    daemon: &DaemonHandle,
    device: &DeviceView,
    icon: &str,
    headline: String,
    detail: String,
    what: &'static str,
) -> gtk::Box {
    let bar = gtk::Box::builder()
        .spacing(14)
        .css_classes(["live-banner"])
        .build();
    bar.append(&icon_square(icon, 28, "live-icon"));
    let words = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .hexpand(true)
        .valign(gtk::Align::Center)
        .build();
    words.append(&text(&headline, &["heading"]));
    words.append(&ellipsized(&detail, &["text-dim"]));
    bar.append(&words);
    bar.update_property(&[gtk::accessible::Property::Label(&format!(
        "{headline}, {detail}"
    ))]);
    let stop = super::soft_button("Stop");
    stop.set_css_classes(&["coral-outline"]);
    let (daemon, view) = (daemon.clone(), device.clone());
    stop.connect_clicked(move |_| send(&daemon, &view, what, false));
    bar.append(&stop);
    bar
}

/// The in-use bars: any capture of this computer's sound (coral, A5), and
/// the phone's sound or microphone playing here.
pub(super) fn bars(daemon: &DaemonHandle, device: &DeviceView) -> Vec<gtk::Box> {
    let Some(sound) = device.sound.as_ref() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    if let Some(source) = sound.sending.as_deref() {
        let (icon, what) = if source == "mic" {
            (
                "audio-input-microphone-symbolic",
                "this computer's microphone",
            )
        } else {
            ("audio-speakers-symbolic", "this computer's sound")
        };
        out.push(bar(
            daemon,
            device,
            icon,
            format!("{} hears {what}", device.name),
            "Sound is being recorded here and sent to the phone".to_string(),
            "desk-sound",
        ));
    }
    if let Some(source) = sound.hearing.as_deref() {
        let mic = source == "mic";
        let headline = match (mic, sound.hearing_live) {
            (true, true) => format!("{}'s microphone is in use", device.name),
            (false, true) => format!("Playing {}'s sound", device.name),
            (_, false) => format!("Asking {} for its sound…", device.name),
        };
        let detail = if !sound.hearing_live {
            "Allow it on the phone".to_string()
        } else if mic {
            format!(
                "As “{}” for apps here",
                crate::device_links::webrtc::audio::MICROPHONE_NODE_DESCRIPTION
            )
        } else {
            "On this computer's speakers".to_string()
        };
        out.push(bar(
            daemon,
            device,
            if mic {
                "audio-input-microphone-symbolic"
            } else {
                "audio-speakers-symbolic"
            },
            headline,
            detail,
            if mic { "phone-mic" } else { "phone-sound" },
        ));
    }
    out
}
