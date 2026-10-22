//! The phone as a webcam in the window (C4): "Use as Webcam…" (front or
//! back, 720p or 1080p) and, while it is asked or live, a coral in-use bar
//! with Flip and Stop.

use adw::prelude::*;

use super::{ellipsized, icon_square, DeviceView};
use crate::device_links::daemon::{DaemonCommand, DaemonHandle};

/// The bar shown while the phone's camera is asked for or in use.
pub(super) fn bar(daemon: &DaemonHandle, device: &DeviceView) -> Option<gtk::Box> {
    let camera = device.camera.as_ref()?;
    let bar = gtk::Box::builder()
        .spacing(14)
        .css_classes(["live-banner"])
        .build();
    bar.append(&icon_square("camera-web-symbolic", 28, "live-icon"));
    let words = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .hexpand(true)
        .valign(gtk::Align::Center)
        .build();
    let (headline, detail) = if camera.live {
        (
            format!("{}'s camera is in use", device.name),
            format!(
                "As “{}” · {}×{} · {}",
                crate::device_links::webrtc::screen::CAMERA_NODE_DESCRIPTION,
                camera.width,
                camera.height,
                camera.facing
            ),
        )
    } else {
        (
            format!("Asking {} for its camera…", device.name),
            "Allow it on the phone".to_string(),
        )
    };
    words.append(&ellipsized(&headline, &["heading"]));
    words.append(&ellipsized(&detail, &["text-dim"]));
    bar.append(&words);
    bar.update_property(&[gtk::accessible::Property::Label(&format!(
        "{headline}, {detail}"
    ))]);
    if camera.live {
        let flip = super::soft_button("Flip");
        flip.set_tooltip_text(Some("Switch between the front and back camera"));
        let (daemon, id) = (daemon.clone(), device.id.clone());
        flip.connect_clicked(move |_| {
            daemon.send(DaemonCommand::CameraControl(id.clone(), "flip".into()))
        });
        bar.append(&flip);
    }
    let stop = super::soft_button(if camera.live { "Stop" } else { "Cancel" });
    stop.set_css_classes(&["coral-outline"]);
    let (daemon, id) = (daemon.clone(), device.id.clone());
    stop.connect_clicked(move |_| {
        daemon.send(DaemonCommand::CameraControl(id.clone(), "stop".into()))
    });
    bar.append(&stop);
    Some(bar)
}

/// "Use as Webcam": which camera and how sharp, then it is asked for.
pub(super) fn ask(daemon: &DaemonHandle, device: &DeviceView, parent: &impl IsA<gtk::Widget>) {
    let back = gtk::ToggleButton::builder()
        .label("Back")
        .active(true)
        .build();
    let front = gtk::ToggleButton::builder()
        .label("Front")
        .group(&back)
        .build();
    let facing = gtk::Box::builder()
        .css_classes(["linked"])
        .halign(gtk::Align::Center)
        .build();
    facing.append(&back);
    facing.append(&front);
    let size = gtk::DropDown::from_strings(&["1280 × 720", "1920 × 1080"]);
    size.set_halign(gtk::Align::Center);
    let extra = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .build();
    extra.append(&facing);
    extra.append(&size);
    let microphone = gtk::CheckButton::builder()
        .label("Its microphone too, as “DeskLink Microphone”")
        .active(true)
        .halign(gtk::Align::Center)
        .build();
    extra.append(&microphone);
    let dialog = adw::AlertDialog::new(
        Some(&format!("Use {} as a Webcam", device.name)),
        Some("Apps here can then pick “DeskLink Camera”. The phone asks you to allow it."),
    );
    dialog.add_responses(&[("cancel", "_Cancel"), ("start", "_Start")]);
    dialog.set_response_appearance("start", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("start"));
    dialog.set_close_response("cancel");
    dialog.set_extra_child(Some(&extra));
    let (daemon, id) = (daemon.clone(), device.id.clone());
    dialog.choose(parent, gtk::gio::Cancellable::NONE, move |response| {
        if response != "start" {
            return;
        }
        let (width, height) = if size.selected() == 1 {
            (1920, 1080)
        } else {
            (1280, 720)
        };
        let facing = if front.is_active() { "front" } else { "back" };
        daemon.send(DaemonCommand::StartCamera(
            id.clone(),
            facing.into(),
            width,
            height,
            microphone.is_active(),
        ));
    });
}
