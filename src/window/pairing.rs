//! Pairing: the welcome page's QR code, "Pair New Device" with the phones
//! nearby, and confirming a phone's request by comparing codes.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::{gdk, glib};

use super::{
    device_status_presentation, device_type_icon, device_type_label, friendly_error, spaced_code,
    text, DesklinkWindow,
};
use crate::device_links::daemon::{DaemonCommand, DaemonHandle};
use crate::device_links::device::{DeviceStatus, DeviceView};

/// The welcome page: a pairing QR code, what to do with it, and pairing by
/// code instead. Returns the flag that stops its code ([`stop_welcome`]).
pub(super) fn welcome_page(win: &DesklinkWindow, daemon: &DaemonHandle) -> Rc<Cell<bool>> {
    let body = win.imp().welcome_body.get();
    while let Some(child) = body.first_child() {
        body.remove(&child);
    }
    let center = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(28)
        .vexpand(true)
        .valign(gtk::Align::Center)
        .halign(gtk::Align::Center)
        .margin_start(24)
        .margin_end(24)
        .margin_bottom(40)
        .build();
    let (code, stopped) = qr_code(daemon);
    let frame = gtk::Box::builder()
        .halign(gtk::Align::Center)
        .css_classes(["qr-card"])
        .build();
    frame.append(&code);
    center.append(&frame);
    let words = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(10)
        .build();
    words.append(
        &gtk::Label::builder()
            .label("Pair a phone")
            .css_classes(["pair-title"])
            .build(),
    );
    words.append(
        &gtk::Label::builder()
            .label("Open DeskLink on your Android phone and scan this code.")
            .wrap(true)
            .max_width_chars(40)
            .justify(gtk::Justification::Center)
            .css_classes(["pair-subtitle"])
            .build(),
    );
    center.append(&words);
    let waiting = gtk::Box::builder()
        .spacing(8)
        .halign(gtk::Align::Center)
        .build();
    waiting.append(
        &gtk::Box::builder()
            .valign(gtk::Align::Center)
            .css_classes(["dot", "accent-dot", "pulse"])
            .accessible_role(gtk::AccessibleRole::Presentation)
            .build(),
    );
    waiting.append(&text("Waiting for a phone…", &["text-dim", "body-small"]));
    center.append(&waiting);
    body.append(&center);

    let footer = gtk::Box::builder()
        .spacing(12)
        .css_classes(["pair-footer"])
        .build();
    let visible = gtk::Box::builder().hexpand(true).build();
    visible.append(&text("Visible as ", &["text-dim", "body-small"]));
    visible.append(&text(&host_name(), &["mono", "body-small"]));
    footer.append(&visible);
    let codes = gtk::Button::builder()
        .label("Pair with a Code Instead")
        .css_classes(["link-button"])
        .build();
    let window = win.downgrade();
    codes.connect_clicked(move |_| {
        if let Some(window) = window.upgrade() {
            window.show_discovery_dialog(false);
        }
    });
    footer.append(&codes);
    body.append(&footer);
    stopped
}

/// Takes the welcome page's code off the air.
pub(super) fn stop_welcome(daemon: &DaemonHandle, stopped: &Cell<bool>) {
    stopped.set(true);
    daemon.send(DaemonCommand::CancelQrPairing);
}

/// This computer's name as phones see it.
fn host_name() -> String {
    hostname::get()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "this computer".to_string())
}

impl DesklinkWindow {
    /// "Pair New Device": the phones found nearby, to pair with by code,
    /// and with `with_qr` a QR code to scan first.
    pub(super) fn show_discovery_dialog(&self, with_qr: bool) {
        let Some(daemon) = self.daemon() else {
            return;
        };
        // Ctrl+N with a dialog already up would stack a second one on it.
        if self.visible_dialog().is_some() {
            return;
        }
        daemon.send(DaemonCommand::Discover);

        let dialog = adw::Dialog::builder()
            .title(if with_qr {
                "Pair New Device"
            } else {
                "Pair with a Code"
            })
            .content_width(460)
            .content_height(if with_qr { 680 } else { 440 })
            .build();
        let toolbar = adw::ToolbarView::new();
        let header = adw::HeaderBar::new();
        let refresh = gtk::Button::builder()
            .icon_name("view-refresh-symbolic")
            .tooltip_text("Search Again")
            .build();
        refresh.update_property(&[gtk::accessible::Property::Label("Search Again")]);
        header.pack_start(&refresh);
        toolbar.add_top_bar(&header);

        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(24)
            .margin_top(12)
            .margin_bottom(24)
            .margin_start(12)
            .margin_end(12)
            .build();
        if with_qr {
            let (code, stopped) = qr_code(&daemon);
            let section = gtk::Box::builder()
                .orientation(gtk::Orientation::Vertical)
                .spacing(12)
                .halign(gtk::Align::Center)
                .build();
            let frame = gtk::Box::builder()
                .halign(gtk::Align::Center)
                .css_classes(["qr-card"])
                .build();
            frame.append(&code);
            section.append(&frame);
            section.append(
                &gtk::Label::builder()
                    .label("Scan with DeskLink on your phone.")
                    .wrap(true)
                    .justify(gtk::Justification::Center)
                    .css_classes(["dim-label"])
                    .build(),
            );
            content.append(&section);
            // Closing withdraws the code, so none outlives what showed it.
            let daemon = daemon.clone();
            dialog.connect_closed(move |_| stop_welcome(&daemon, &stopped));
        }

        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();
        let nearby = adw::PreferencesGroup::builder()
            .title("Nearby Devices")
            .description(
                "Phones on this network with DeskLink open. Pair, then check both show the same code.",
            )
            .header_suffix(
                &gtk::Spinner::builder()
                    .spinning(true)
                    .valign(gtk::Align::Center)
                    .build(),
            )
            .build();
        nearby.add(&list);
        content.append(&nearby);

        toolbar.set_content(Some(
            &gtk::ScrolledWindow::builder()
                .hscrollbar_policy(gtk::PolicyType::Never)
                .child(
                    &adw::Clamp::builder()
                        .maximum_size(400)
                        .child(&content)
                        .build(),
                )
                .build(),
        ));
        dialog.set_child(Some(&toolbar));

        refresh.connect_clicked({
            let daemon = daemon.clone();
            move |_| daemon.send(DaemonCommand::Discover)
        });

        let snapshot = Rc::new(RefCell::new(None));
        populate_discovery_list(&list, &daemon, &snapshot);
        let weak_list = list.downgrade();
        glib::timeout_add_seconds_local(1, move || {
            let Some(list) = weak_list.upgrade() else {
                return glib::ControlFlow::Break;
            };
            populate_discovery_list(&list, &daemon, &snapshot);
            glib::ControlFlow::Continue
        });

        dialog.present(Some(self));
    }
}

/// Rebuilds the nearby list, but only when its contents changed.
///
/// A rebuild every second destroyed a "Pair" button between mouse-down and
/// mouse-up, so the press was silently lost. Paired devices are left out:
/// they belong in the switcher, in a dialog whose purpose is pairing
/// something new.
fn populate_discovery_list(
    list: &gtk::ListBox,
    daemon: &DaemonHandle,
    last_snapshot: &RefCell<Option<String>>,
) {
    #[allow(unused_mut)]
    let mut devices: Vec<_> = daemon
        .devices()
        .into_iter()
        .filter(|device| !device.trusted)
        .collect();
    #[cfg(debug_assertions)]
    if let Some(window) = list.root().and_downcast::<DesklinkWindow>() {
        devices.extend(
            window
                .imp()
                .fake_devices
                .borrow()
                .iter()
                .filter(|device| !device.trusted)
                .cloned(),
        );
    }

    let snapshot = serde_json::to_string(&devices).unwrap_or_default();
    if last_snapshot.borrow().as_ref() == Some(&snapshot) {
        return;
    }
    last_snapshot.replace(Some(snapshot));
    list.remove_all();
    if devices.is_empty() {
        list.append(
            &adw::ActionRow::builder()
                .title("Looking for phones…")
                .subtitle("Keep both devices on the same Wi-Fi network.")
                .build(),
        );
        return;
    }
    for device in devices {
        list.append(&discovery_row(daemon, device));
    }
}

/// A device in the nearby list: discovered, or mid-pairing with its code.
fn discovery_row(daemon: &DaemonHandle, device: DeviceView) -> adw::ActionRow {
    let mut subtitle_parts: Vec<String> = vec![device_type_label(&device.device_type)];
    if !matches!(device.status, DeviceStatus::Discovered) {
        subtitle_parts.push(device_status_presentation(&device).0.to_string());
    }
    if let Some(error) = &device.last_error {
        subtitle_parts.push(friendly_error(error).0);
    }
    // A phone's name is text, not markup: the row reads it as typed before
    // anything is set on it.
    let row = adw::ActionRow::builder()
        .title(&device.name)
        .use_markup(false)
        .build();
    row.set_subtitle(&subtitle_parts.join(" · "));
    row.set_activatable(false);
    row.add_prefix(&super::icon_square(
        device_type_icon(&device.device_type),
        32,
        "row-icon",
    ));
    let code = |key: &str| {
        gtk::Label::builder()
            .label(spaced_code(key))
            .valign(gtk::Align::Center)
            .css_classes(["mono", "heading", "accent"])
            .build()
    };
    match &device.status {
        DeviceStatus::Discovered | DeviceStatus::Connected | DeviceStatus::Unreachable => {
            let pair = gtk::Button::builder()
                .label("Pair")
                .valign(gtk::Align::Center)
                .build();
            let (daemon, id) = (daemon.clone(), device.id.clone());
            pair.connect_clicked(move |_| daemon.send(DaemonCommand::RequestPair(id.clone())));
            row.add_suffix(&pair);
        }
        DeviceStatus::PairRequested => {
            if let Some(key) = &device.verification_key {
                row.add_suffix(&code(key));
            }
            row.add_suffix(
                &gtk::Spinner::builder()
                    .spinning(true)
                    .valign(gtk::Align::Center)
                    .build(),
            );
        }
        DeviceStatus::PairRequestedByPeer => {
            if let Some(key) = &device.verification_key {
                row.add_suffix(&code(key));
            }
            let reject = gtk::Button::builder()
                .label("Cancel")
                .valign(gtk::Align::Center)
                .build();
            let (sender, id) = (daemon.clone(), device.id.clone());
            reject.connect_clicked(move |_| {
                eprintln!("[UI] Reject clicked for {}", id);
                sender.send(DaemonCommand::RejectPair(id.clone()));
            });
            row.add_suffix(&reject);
            let accept = gtk::Button::builder()
                .label("Pair")
                .valign(gtk::Align::Center)
                .css_classes(["suggested-action"])
                .build();
            let (daemon, id, shown_key) = (
                daemon.clone(),
                device.id.clone(),
                device.verification_key.clone(),
            );
            accept.connect_clicked(move |_| {
                debug_log!("[UI] Accept clicked for {}", id);
                daemon.send(DaemonCommand::AcceptPair(id.clone(), shown_key.clone()));
            });
            row.add_suffix(&accept);
        }
        DeviceStatus::Paired => {}
    }
    row
}

/// The code to compare, large, in mono, and read out digit by digit.
fn pairing_code_label(code: &str) -> gtk::Label {
    let label = gtk::Label::builder()
        .label(spaced_code(code))
        .css_classes(["mono", "pairing-code"])
        .build();
    label.update_property(&[gtk::accessible::Property::Label(&format!(
        "Verification code {}",
        code.chars()
            .map(|digit| digit.to_string())
            .collect::<Vec<_>>()
            .join(" ")
    ))]);
    label
}

/// "Pair with Pixel 9?" with the code to compare. Cancel refuses; Escape
/// only puts the dialog away, and the request stays answerable on the
/// device's overview.
pub(super) fn present_pair_dialog(
    win: &DesklinkWindow,
    daemon: &DaemonHandle,
    device: &DeviceView,
) {
    let dialog = adw::AlertDialog::new(
        Some(&format!("Pair with {}?", device.name)),
        Some("Check that your phone shows the same code."),
    );
    dialog.add_responses(&[("reject", "_Cancel"), ("accept", "_Pair")]);
    dialog.set_response_appearance("accept", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("reject"));
    dialog.set_close_response("later");
    if let Some(key) = &device.verification_key {
        let extra = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(4)
            .build();
        extra.append(&pairing_code_label(key));
        let mut about = device.name.to_lowercase().replace(' ', "-");
        if !device.address.is_empty() {
            about.push_str(&format!(" · {}", device.address));
        }
        extra.append(
            &gtk::Label::builder()
                .label(about)
                .css_classes(["mono", "caption", "text-faint"])
                .build(),
        );
        dialog.set_extra_child(Some(&extra));
    }
    {
        let daemon = daemon.clone();
        let id = device.id.clone();
        let shown_key = device.verification_key.clone();
        let window = win.downgrade();
        dialog.connect_response(None, move |dialog, response| {
            match response {
                "accept" => {
                    debug_log!("[UI] Accept clicked for {}", id);
                    daemon.send(DaemonCommand::AcceptPair(id.clone(), shown_key.clone()));
                }
                "reject" => {
                    eprintln!("[UI] Reject clicked for {}", id);
                    daemon.send(DaemonCommand::RejectPair(id.clone()));
                }
                _ => {}
            }
            if let Some(window) = window.upgrade() {
                let mut dialogs = window.imp().pair_dialogs.borrow_mut();
                if dialogs.get(&id).is_some_and(|(_, shown)| shown == dialog) {
                    dialogs.remove(&id);
                }
            }
        });
    }
    if let Some((_, old)) = win.imp().pair_dialogs.borrow_mut().insert(
        device.id.clone(),
        (device.verification_key.clone(), dialog.clone()),
    ) {
        old.force_close();
    }
    dialog.present(Some(win));
}

/// The phone asked to pair: the code to compare, and Cancel / Pair.
pub(super) fn pair_request_view(daemon: &DaemonHandle, device: &DeviceView) -> gtk::Box {
    let view = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .margin_top(48)
        .halign(gtk::Align::Center)
        .build();
    view.append(
        &gtk::Label::builder()
            .label(format!("Pair with {}?", device.name))
            .wrap(true)
            .justify(gtk::Justification::Center)
            .css_classes(["pair-title"])
            .build(),
    );
    view.append(
        &gtk::Label::builder()
            .label("Check that your phone shows the same code.")
            .wrap(true)
            .justify(gtk::Justification::Center)
            .css_classes(["text-dim"])
            .build(),
    );
    if let Some(key) = &device.verification_key {
        view.append(&pairing_code_label(key));
    }
    let buttons = gtk::Box::builder()
        .spacing(12)
        .halign(gtk::Align::Center)
        .homogeneous(true)
        .margin_top(18)
        .build();
    let reject = gtk::Button::builder()
        .label("Cancel")
        .width_request(150)
        .css_classes(["soft-button"])
        .build();
    let accept = gtk::Button::builder()
        .label("Pair")
        .css_classes(["suggested-action"])
        .build();
    let (sender, id) = (daemon.clone(), device.id.clone());
    reject.connect_clicked(move |_| {
        eprintln!("[UI] Reject clicked for {}", id);
        sender.send(DaemonCommand::RejectPair(id.clone()));
    });
    let (daemon, id, shown_key) = (
        daemon.clone(),
        device.id.clone(),
        device.verification_key.clone(),
    );
    accept.connect_clicked(move |_| {
        debug_log!("[UI] Accept clicked for {}", id);
        daemon.send(DaemonCommand::AcceptPair(id.clone(), shown_key.clone()));
    });
    buttons.append(&reject);
    buttons.append(&accept);
    view.append(&buttons);
    view
}

/// This computer asked to pair: waiting for the phone to accept.
pub(super) fn pair_waiting_view(device: &DeviceView) -> gtk::Box {
    let view = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .margin_top(48)
        .halign(gtk::Align::Center)
        .build();
    let heading = gtk::Box::builder()
        .spacing(12)
        .halign(gtk::Align::Center)
        .build();
    heading.append(&gtk::Spinner::builder().spinning(true).build());
    heading.append(
        &gtk::Label::builder()
            .label(format!("Waiting for {}", device.name))
            .wrap(true)
            .css_classes(["pair-title"])
            .build(),
    );
    view.append(&heading);
    view.append(
        &gtk::Label::builder()
            .label(if device.verification_key.is_some() {
                "Accept on the phone if it shows this code."
            } else {
                "Accept the request on the phone."
            })
            .wrap(true)
            .justify(gtk::Justification::Center)
            .css_classes(["text-dim"])
            .build(),
    );
    if let Some(key) = &device.verification_key {
        view.append(&pairing_code_label(key));
    }
    view
}

/// A live pairing QR code from `pairing_qr`, and the flag that takes it
/// off the air. A scanned or expired code is replaced by a fresh one.
fn qr_code(daemon: &DaemonHandle) -> (gtk::Picture, Rc<Cell<bool>>) {
    let picture = gtk::Picture::builder()
        .can_shrink(false)
        .halign(gtk::Align::Center)
        .build();
    picture.update_property(&[gtk::accessible::Property::Label(
        "Pairing QR code. Scan it with DeskLink on your phone.",
    )]);
    daemon.send(DaemonCommand::StartQrPairing);
    let shown = Rc::new(RefCell::new(None::<String>));
    // Set when the code is withdrawn, so a tick racing it can never start
    // a code nothing shows (it would stay live for 5 minutes).
    let stopped = Rc::new(Cell::new(false));
    let tick_stopped = Rc::clone(&stopped);
    // Not yet on screen counts as on screen: the code was just asked for.
    let paused = Cell::new(false);
    let weak_picture = picture.downgrade();
    let poll_daemon = daemon.clone();
    let step = Rc::new(move || {
        let Some(picture) = weak_picture.upgrade() else {
            return glib::ControlFlow::Break;
        };
        if tick_stopped.get() {
            return glib::ControlFlow::Break;
        }
        // Off screen (the window closed to the background, another page
        // showing): no code stays live that nobody can see.
        if !picture.is_mapped() {
            if !paused.replace(true) {
                shown.borrow_mut().take();
                poll_daemon.send(DaemonCommand::CancelQrPairing);
            }
            // Stops until the picture is shown again (its map starts it).
            return glib::ControlFlow::Break;
        }
        if paused.replace(false) {
            poll_daemon.send(DaemonCommand::StartQrPairing);
            return glib::ControlFlow::Continue;
        }
        // A phone scanned a code that has run out: say so, and show the new one.
        if crate::device_links::pairing_qr::take_expired_notice() {
            super::toast_from(&picture, "That code has expired — scan the new one");
            if shown.borrow_mut().take().is_some() {
                poll_daemon.send(DaemonCommand::StartQrPairing);
            }
            return glib::ControlFlow::Continue;
        }
        match crate::device_links::pairing_qr::current_uri() {
            Some(uri) if shown.borrow().as_deref() != Some(uri.as_str()) => {
                if let Some(texture) = qr_texture(&uri) {
                    picture.set_paintable(Some(&texture));
                }
                *shown.borrow_mut() = Some(uri);
            }
            Some(_) => {}
            None => {
                // Used, expired or withdrawn by another view: a new one.
                if shown.borrow_mut().take().is_some() {
                    poll_daemon.send(DaemonCommand::StartQrPairing);
                }
            }
        }
        glib::ControlFlow::Continue
    });
    // A quarter-second look while the code is on screen, and none while it
    // is not.
    let running = Rc::new(Cell::new(false));
    let start: Rc<dyn Fn()> = Rc::new(move || {
        if running.replace(true) {
            return;
        }
        let (step, running) = (Rc::clone(&step), Rc::clone(&running));
        glib::timeout_add_local(std::time::Duration::from_millis(250), move || {
            let flow = step();
            if flow == glib::ControlFlow::Break {
                running.set(false);
            }
            flow
        });
    });
    start();
    picture.connect_map(move |_| start());
    (picture, stopped)
}

/// Rasterises a QR code about 170 px across. Its card's white padding
/// (20 px) plus one module make the standard 4-module quiet zone at the
/// scales used here (3 to 6 px a module).
fn qr_texture(uri: &str) -> Option<gdk::MemoryTexture> {
    const QUIET: usize = 1;
    let (width, dark) = crate::device_links::pairing_qr::modules(uri).ok()?;
    let scale = (170 / width.max(1)).clamp(3, 6);
    let side = (width + 2 * QUIET) * scale;
    let mut pixels = vec![255u8; side * side * 3];
    for (index, is_dark) in dark.iter().enumerate() {
        if !is_dark {
            continue;
        }
        let (row, column) = (index / width + QUIET, index % width + QUIET);
        for y in row * scale..(row + 1) * scale {
            let start = (y * side + column * scale) * 3;
            pixels[start..start + scale * 3].fill(0);
        }
    }
    Some(gdk::MemoryTexture::new(
        side as i32,
        side as i32,
        gdk::MemoryFormat::R8g8b8,
        &glib::Bytes::from_owned(pixels),
        side * 3,
    ))
}
