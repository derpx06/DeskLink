//! The phone's camera in the window: the overview's Camera action, a
//! popover with the webcam (C4) and Insert from the phone (F3) — a photo, a
//! scanned document, the text in a picture or a QR code — and a bar while a
//! capture waits for the phone, with Cancel.

use adw::prelude::*;

use super::{ellipsized, icon_square, text, DeviceView};
use crate::device_links::daemon::{DaemonCommand, DaemonHandle};

/// What each row asks for: kind, target, label, icon.
const CAPTURES: [(&str, &str, &str, &str); 5] = [
    (
        "photo",
        "clipboard",
        "Take a Photo",
        "camera-photo-symbolic",
    ),
    (
        "photo",
        "file",
        "Take a Photo as a File",
        "document-save-symbolic",
    ),
    ("scan", "file", "Scan a Document (PDF)", "scanner-symbolic"),
    (
        "text",
        "clipboard",
        "Copy Text from a Picture",
        "format-text-plain-symbolic",
    ),
    ("qr", "clipboard", "Scan a QR Code", "view-grid-symbolic"),
];

fn row(icon: &str, label: &str) -> gtk::Button {
    let content = gtk::Box::builder().spacing(12).build();
    content.append(&gtk::Image::from_icon_name(icon));
    content.append(&gtk::Label::builder().label(label).xalign(0.0).build());
    gtk::Button::builder()
        .child(&content)
        .css_classes(["flat"])
        .build()
}

/// Attaches the Camera popover to the overview's round action.
pub(super) fn attach_menu(daemon: &DaemonHandle, device: &DeviceView, button: &gtk::Button) {
    let list = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .margin_top(6)
        .margin_bottom(6)
        .margin_start(6)
        .margin_end(6)
        .build();
    let popover = gtk::Popover::builder().child(&list).build();
    if device.camera.is_none() {
        let webcam = row("camera-web-symbolic", "Use as Webcam…");
        let (daemon, view, popover, anchor) = (
            daemon.clone(),
            device.clone(),
            popover.clone(),
            button.clone(),
        );
        webcam.connect_clicked(move |_| {
            popover.popdown();
            super::camera::ask(&daemon, &view, &anchor);
        });
        list.append(&webcam);
        list.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    }
    for (kind, target, label, icon) in CAPTURES {
        let item = row(icon, label);
        let (daemon, id, popover) = (daemon.clone(), device.id.clone(), popover.clone());
        item.connect_clicked(move |_| {
            popover.popdown();
            daemon.send(DaemonCommand::Capture(
                id.clone(),
                kind.into(),
                target.into(),
            ));
        });
        list.append(&item);
    }
    popover.set_parent(button);
    button.connect_destroy({
        let popover = popover.clone();
        move |_| popover.unparent()
    });
    button.connect_clicked(move |_| popover.popup());
}

/// The bar while a capture waits for the phone.
pub(super) fn bar(daemon: &DaemonHandle, device: &DeviceView) -> Option<gtk::Box> {
    let capture = device.capture.as_ref()?;
    let what = match capture.kind.as_str() {
        "scan" => "a scanned document",
        "text" => "the text in a picture",
        "qr" => "a QR code",
        _ => "a photo",
    };
    let bar = gtk::Box::builder()
        .spacing(14)
        .css_classes(["state-banner"])
        .build();
    bar.append(&icon_square("camera-photo-symbolic", 28, "live-icon"));
    let words = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .hexpand(true)
        .valign(gtk::Align::Center)
        .build();
    let (headline, detail) = if capture.receiving {
        (
            format!("Receiving {what} from {}…", device.name),
            String::new(),
        )
    } else {
        (
            format!("Asking {} for {what}…", device.name),
            "Continue on the phone".to_string(),
        )
    };
    words.append(&text(&headline, &["heading"]));
    if !detail.is_empty() {
        words.append(&ellipsized(&detail, &["text-dim"]));
    }
    bar.append(&words);
    bar.update_property(&[gtk::accessible::Property::Label(&format!(
        "{headline} {detail}"
    ))]);
    let cancel = super::soft_button("Cancel");
    let (daemon, id) = (daemon.clone(), device.id.clone());
    cancel.connect_clicked(move |_| {
        daemon.send(DaemonCommand::Capture(
            id.clone(),
            "cancel".into(),
            String::new(),
        ))
    });
    bar.append(&cancel);
    Some(bar)
}
