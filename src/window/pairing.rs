//! Pairing: the welcome page's QR code, "Pair another phone" with the phones
//! nearby, and confirming a phone's request by comparing codes.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::{gdk, glib};

use super::ui_kit::banner::{banner, empty_state, BannerKind};
use super::ui_kit::{button, icon_button, icon_square, ButtonKind, Tone};
use super::{
    device_type_icon, device_type_label, friendly_error, hero_error_words, spaced_code, text,
    DesklinkWindow,
};
use crate::device_links::daemon::{DaemonCommand, DaemonHandle};
use crate::device_links::device::{DeviceStatus, DeviceView};
use crate::link_glyph::Presentation;

/// The titles of the pairing dialog (the window closes it when a scan pairs).
pub(super) const PAIR_TITLE: &str = "Pair another phone";
pub(super) const PAIR_CODE_TITLE: &str = "Pair with a code";

/// What the welcome page says to do.
pub(super) const WELCOME_HELP: &str = "Open DeskLink on your phone and scan this code.";
/// Where to get the phone app (J16).
pub(super) const INSTALL_ADDRESS: &str = "github.com/derpx06/DeskLink/releases";

/// The welcome page: a pairing QR code, what to do with it, and pairing by
/// code instead. Returns the flag that stops its code ([`stop_welcome`]).
pub(super) fn welcome_page(win: &DesklinkWindow, daemon: &DaemonHandle) -> Rc<Cell<bool>> {
    let body = win.imp().welcome_body.get();
    while let Some(child) = body.first_child() {
        body.remove(&child);
    }
    let center = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(24)
        .vexpand(true)
        .valign(gtk::Align::Center)
        .halign(gtk::Align::Center)
        .margin_start(24)
        .margin_end(24)
        .margin_bottom(32)
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
        .spacing(8)
        .build();
    words.append(
        &gtk::Label::builder()
            .label("Pair a phone")
            .wrap(true)
            .justify(gtk::Justification::Center)
            .css_classes(["pair-title"])
            .accessible_role(gtk::AccessibleRole::Heading)
            .build(),
    );
    words.append(
        &gtk::Label::builder()
            .label(WELCOME_HELP)
            .wrap(true)
            .max_width_chars(40)
            .justify(gtk::Justification::Center)
            .css_classes(["pair-subtitle"])
            .build(),
    );
    center.append(&words);
    center.append(&install_line());
    let waiting = gtk::Box::builder()
        .spacing(8)
        .halign(gtk::Align::Center)
        .build();
    let dots = gtk::Box::builder()
        .spacing(4)
        .valign(gtk::Align::Center)
        .accessible_role(gtk::AccessibleRole::Presentation)
        .build();
    for delay in ["one", "two", "three"] {
        dots.append(
            &gtk::Box::builder()
                .valign(gtk::Align::Center)
                .css_classes(["dot", "pulse", delay])
                .accessible_role(gtk::AccessibleRole::Presentation)
                .build(),
        );
    }
    waiting.append(&dots);
    waiting.append(&text("Waiting for a phone…", &["text-dim", "body-small"]));
    center.append(&waiting);
    // Scrolls, rather than clipping, in a short window.
    body.append(
        &gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .propagate_natural_height(false)
            .child(&center)
            .build(),
    );

    let footer = gtk::Box::builder()
        .spacing(12)
        .css_classes(["pair-footer"])
        .build();
    // Under each other when the window is phone-narrow.
    footer.set_widget_name("responsive");
    let visible = gtk::Box::builder()
        .spacing(6)
        .hexpand(true)
        .valign(gtk::Align::Center)
        .build();
    visible.append(&text("Visible as", &["text-dim", "body-small"]));
    // Cut in the middle (whole on hover) rather than pushing a narrow
    // window with large text wider than the screen.
    let host = super::ellipsized(&host_name(), &["mono", "body-small"]);
    host.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    visible.append(&host);
    footer.append(&visible);
    let codes = button(ButtonKind::Text, "Pair with a code instead", None);
    codes.set_halign(gtk::Align::End);
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

/// "No app yet?" and the release address, selectable, with a copy button.
fn install_line() -> gtk::Box {
    let line = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(4)
        .halign(gtk::Align::Center)
        .build();
    line.append(
        &gtk::Label::builder()
            .label("No app yet? Get DeskLink for Android at")
            .wrap(true)
            .justify(gtk::Justification::Center)
            .css_classes(["text-dim", "body-small"])
            .build(),
    );
    let address = gtk::Box::builder()
        .spacing(4)
        .halign(gtk::Align::Center)
        .build();
    address.append(
        &gtk::Label::builder()
            .label(INSTALL_ADDRESS)
            .valign(gtk::Align::Center)
            // Large text in a narrow window: cut in the middle (Copy has it
            // whole) rather than pushing the page past the window's edge.
            .ellipsize(gtk::pango::EllipsizeMode::Middle)
            .tooltip_text(INSTALL_ADDRESS)
            .css_classes(["dl-mono", "install-address"])
            .build(),
    );
    let copy = icon_button("desklink-content-copy-symbolic", "Copy the address");
    copy.connect_clicked(|copy| {
        copy.display().clipboard().set_text(INSTALL_ADDRESS);
        super::toast_from(copy, "Address copied");
    });
    address.append(&copy);
    line.append(&address);
    line
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
    /// "Pair another phone": the phones found nearby, to pair with by code,
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
            .title(if with_qr { PAIR_TITLE } else { PAIR_CODE_TITLE })
            .content_width(520)
            .content_height(if with_qr { 680 } else { 440 })
            .build();
        let toolbar = adw::ToolbarView::new();
        let header = adw::HeaderBar::new();
        let refresh = icon_button("desklink-refresh-symbolic", "Search again");
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
                    .css_classes(["text-dim"])
                    .build(),
            );
            content.append(&section);
            // Closing withdraws the code, so none outlives what showed it.
            let daemon = daemon.clone();
            dialog.connect_closed(move |_| stop_welcome(&daemon, &stopped));
        }

        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["dl-row-group"])
            .build();
        list.set_placeholder(Some(&empty_state(
            "desklink-search-symbolic",
            "Looking for phones…",
            "Keep both devices on the same Wi-Fi network.",
        )));
        let nearby = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(8)
            .build();
        let heading = gtk::Box::builder().spacing(8).build();
        heading.append(&super::ui_kit::section_header("Phones on this network"));
        heading.append(
            &gtk::Spinner::builder()
                .spinning(true)
                .valign(gtk::Align::Center)
                .build(),
        );
        nearby.append(&heading);
        nearby.append(
            &gtk::Label::builder()
                .label("Pair, then check that both devices show the same code.")
                .xalign(0.0)
                .wrap(true)
                .css_classes(["dl-secondary"])
                .build(),
        );
        nearby.append(&list);
        content.append(&nearby);

        toolbar.set_content(Some(
            &gtk::ScrolledWindow::builder()
                .hscrollbar_policy(gtk::PolicyType::Never)
                .child(
                    &adw::Clamp::builder()
                        .maximum_size(460)
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
    for device in devices {
        list.append(&discovery_row(daemon, device));
    }
}

/// The line under a nearby phone's name: "Phone · Wants to pair · why not".
pub(super) fn discovery_subtitle(device: &DeviceView) -> String {
    let mut parts: Vec<String> = vec![device_type_label(&device.device_type)];
    if !matches!(device.status, DeviceStatus::Discovered) {
        parts.push(Presentation::of(device).detail);
    }
    if let Some(error) = &device.last_error {
        parts.push(friendly_error(error).0);
    }
    parts.retain(|part| !part.is_empty());
    parts.join(" · ")
}

/// A device in the nearby list: discovered, or mid-pairing with its code.
fn discovery_row(daemon: &DaemonHandle, device: DeviceView) -> adw::ActionRow {
    // `list_row` escapes the name: a phone's name is text, not markup.
    let row = super::ui_kit::list_row(
        device_type_icon(&device.device_type),
        Tone::Neutral,
        &device.name,
        Some(&discovery_subtitle(&device)),
    );
    row.set_activatable(false);
    let code = |key: &str| {
        gtk::Label::builder()
            .label(spaced_code(key))
            .valign(gtk::Align::Center)
            .css_classes(["dl-mono", "heading", "accent"])
            .build()
    };
    match &device.status {
        DeviceStatus::Discovered | DeviceStatus::Connected | DeviceStatus::Unreachable => {
            let pair = button(ButtonKind::Tonal, "Pair", None);
            pair.set_valign(gtk::Align::Center);
            pair.update_property(&[gtk::accessible::Property::Label(&format!(
                "Pair with {}",
                device.name
            ))]);
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
            // The code over its two buttons, so the phone's name keeps its
            // room in a narrow dialog.
            let stack = gtk::Box::builder()
                .orientation(gtk::Orientation::Vertical)
                .spacing(6)
                .valign(gtk::Align::Center)
                .halign(gtk::Align::End)
                .build();
            if let Some(key) = &device.verification_key {
                let key = code(key);
                key.set_halign(gtk::Align::End);
                stack.append(&key);
            }
            let buttons = gtk::Box::builder()
                .spacing(6)
                .halign(gtk::Align::End)
                .build();
            let reject = button(ButtonKind::Text, "Cancel", None);
            let (sender, id) = (daemon.clone(), device.id.clone());
            reject.connect_clicked(move |_| {
                eprintln!("[UI] Reject clicked for {}", id);
                sender.send(DaemonCommand::RejectPair(id.clone()));
            });
            buttons.append(&reject);
            let accept = button(ButtonKind::Tonal, "Pair", None);
            let (daemon, id, shown_key) = (
                daemon.clone(),
                device.id.clone(),
                device.verification_key.clone(),
            );
            accept.connect_clicked(move |_| {
                debug_log!("[UI] Accept clicked for {}", id);
                daemon.send(DaemonCommand::AcceptPair(id.clone(), shown_key.clone()));
            });
            buttons.append(&accept);
            stack.append(&buttons);
            row.add_suffix(&stack);
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

/// The phone asking, as a glyph, its name, and its kind and address.
fn phone_line(device: &DeviceView) -> gtk::Box {
    let line = gtk::Box::builder()
        .spacing(12)
        .halign(gtk::Align::Center)
        .build();
    line.append(&icon_square(
        device_type_icon(&device.device_type),
        Tone::Accent,
    ));
    let words = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .valign(gtk::Align::Center)
        .build();
    words.append(&super::ellipsized(&device.name, &["heading"]));
    let mut about = device_type_label(&device.device_type);
    if !device.address.is_empty() {
        about.push_str(&format!(" · {}", device.address));
    }
    words.append(&super::ellipsized(&about, &["dl-mono", "dl-caption"]));
    line.append(&words);
    line
}

/// How long a phone that did not accept our request stays in the list, with
/// "Couldn't pair" and Try again.
const REFUSED_STAYS: std::time::Duration = std::time::Duration::from_secs(120);

/// Remembers the requests this computer made, so one that ends without a
/// pairing (the phone said no, or let it run out) can be shown: the daemon
/// takes such a phone back to "nearby", where the window lists nothing.
#[derive(Debug, Default)]
pub(super) struct PairWatch {
    /// Phones asked now, as last seen.
    asked: std::collections::HashMap<String, DeviceView>,
    /// Phones that did not accept, and when we noticed.
    refused: std::collections::HashMap<String, (DeviceView, std::time::Instant)>,
}

impl PairWatch {
    /// Looks at the devices the daemon knows. Returns the phones to keep
    /// showing as refused, and the names of those newly refused. Calling it
    /// again with the same devices changes nothing.
    pub(super) fn observe(
        &mut self,
        devices: &[DeviceView],
        now: std::time::Instant,
    ) -> (Vec<DeviceView>, Vec<String>) {
        let mut newly = Vec::new();
        for id in std::mem::take(&mut self.asked).into_keys() {
            let now_device = devices.iter().find(|device| device.id == id);
            match now_device {
                // Paired, or still being asked: nothing to report.
                Some(device) if device.trusted || device.status == DeviceStatus::PairRequested => {}
                // Asked, and now a stranger again (or gone): it did not accept.
                Some(device) if device.status != DeviceStatus::PairRequestedByPeer => {
                    let mut gone = device.clone();
                    gone.last_error
                        .get_or_insert_with(|| "Pairing request was rejected".into());
                    newly.push(gone.name.clone());
                    self.refused.insert(id, (gone, now));
                }
                _ => {}
            }
        }
        for device in devices {
            if device.trusted {
                self.refused.remove(&device.id);
            } else if device.status == DeviceStatus::PairRequested {
                self.refused.remove(&device.id);
                self.asked.insert(device.id.clone(), device.clone());
            } else if device.status == DeviceStatus::PairRequestedByPeer {
                self.refused.remove(&device.id);
            }
        }
        self.refused
            .retain(|_, (_, at)| now.duration_since(*at) < REFUSED_STAYS);
        let kept = self
            .refused
            .values()
            .map(|(device, _)| device.clone())
            .collect();
        (kept, newly)
    }
}

thread_local! {
    static WATCH: RefCell<PairWatch> = RefCell::default();
}

/// [`PairWatch::observe`] for the window (one thread, one watch).
pub(super) fn watch(devices: &[DeviceView]) -> (Vec<DeviceView>, Vec<String>) {
    WATCH.with(|watch| {
        watch
            .borrow_mut()
            .observe(devices, std::time::Instant::now())
    })
}

/// The reminder under every pairing question.
const OWN_PHONES_ONLY: &str = "Only pair phones you own.";

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
        Some("Check that this code is also on the phone."),
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
        extra.append(&phone_line(device));
        extra.append(&pairing_code_label(key));
        extra.append(
            &gtk::Label::builder()
                .label(OWN_PHONES_ONLY)
                .css_classes(["dl-caption"])
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

/// A centred column with the phone, a title and a line of help: the shape
/// of every pairing page of a phone that is not paired yet.
fn pair_page(device: &DeviceView, title: &str, help: &str) -> gtk::Box {
    let view = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .margin_top(48)
        .halign(gtk::Align::Center)
        .build();
    view.append(&phone_line(device));
    view.append(
        &gtk::Label::builder()
            .label(title)
            .wrap(true)
            .justify(gtk::Justification::Center)
            .css_classes(["pair-title"])
            .accessible_role(gtk::AccessibleRole::Heading)
            .build(),
    );
    view.append(
        &gtk::Label::builder()
            .label(help)
            .wrap(true)
            .justify(gtk::Justification::Center)
            .css_classes(["text-dim"])
            .build(),
    );
    view
}

/// The phone asked to pair: the code to compare, and Cancel / Pair.
pub(super) fn pair_request_view(daemon: &DaemonHandle, device: &DeviceView) -> gtk::Box {
    let view = pair_page(
        device,
        &format!("Pair with {}?", device.name),
        "Check that this code is also on the phone.",
    );
    if let Some(key) = &device.verification_key {
        view.append(&pairing_code_label(key));
    }
    let buttons = gtk::Box::builder()
        .spacing(12)
        .halign(gtk::Align::Center)
        .homogeneous(true)
        .margin_top(12)
        .build();
    let reject = button(ButtonKind::Tonal, "Cancel", None);
    let accept = button(ButtonKind::Primary, "Pair", None);
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
    view.append(
        &gtk::Label::builder()
            .label(OWN_PHONES_ONLY)
            .css_classes(["dl-caption"])
            .build(),
    );
    view
}

/// This computer asked to pair: waiting for the phone to accept. Cancel
/// withdraws the request.
pub(super) fn pair_waiting_view(daemon: &DaemonHandle, device: &DeviceView) -> gtk::Box {
    let view = pair_page(
        device,
        &format!("Waiting for {}", device.name),
        if device.verification_key.is_some() {
            "Accept on the phone if it shows this code."
        } else {
            "Accept the request on the phone."
        },
    );
    view.append(&gtk::Spinner::builder().spinning(true).build());
    if let Some(key) = &device.verification_key {
        view.append(&pairing_code_label(key));
    }
    let cancel = button(ButtonKind::Text, "Cancel", None);
    cancel.set_halign(gtk::Align::Center);
    let (daemon, id) = (daemon.clone(), device.id.clone());
    cancel.connect_clicked(move |_| daemon.send(DaemonCommand::RejectPair(id.clone())));
    view.append(&cancel);
    view
}

/// A phone that is nearby (or refused, or let the request expire): Pair, and
/// when the last try did not go through, why and Try again.
pub(super) fn pair_idle_view(daemon: &DaemonHandle, device: &DeviceView) -> gtk::Box {
    let view = pair_page(
        device,
        &format!("Pair with {}", device.name),
        "Both devices show the same code. Check it before you accept.",
    );
    if let Some(error) = &device.last_error {
        let (words, _) = hero_error_words(error);
        let failed = banner(
            BannerKind::Error,
            "desklink-warning-symbolic",
            &format!("Couldn't pair with {}", device.name),
            Some(&words),
        );
        failed.root.set_halign(gtk::Align::Center);
        let again = button(ButtonKind::Text, "Try again", None);
        let (daemon, id) = (daemon.clone(), device.id.clone());
        again.connect_clicked(move |_| daemon.send(DaemonCommand::RequestPair(id.clone())));
        failed.add_action(&again);
        view.append(&failed.root);
        return view;
    }
    let pair = button(ButtonKind::Primary, "Pair", None);
    pair.set_halign(gtk::Align::Center);
    let (daemon, id) = (daemon.clone(), device.id.clone());
    pair.connect_clicked(move |_| daemon.send(DaemonCommand::RequestPair(id.clone())));
    view.append(&pair);
    view
}

/// A live pairing QR code from `pairing_qr`, and the flag that takes it
/// off the air. A scanned or expired code is replaced by a fresh one.
fn qr_code(daemon: &DaemonHandle) -> (gtk::Picture, Rc<Cell<bool>>) {
    // Its size is kept while the code is being made, so the page does not
    // jump when it arrives (the picture is about 175 px).
    let picture = gtk::Picture::builder()
        .can_shrink(false)
        .halign(gtk::Align::Center)
        .width_request(175)
        .height_request(175)
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

#[cfg(test)]
mod welcome_tests {
    use super::*;
    use crate::device_links::device_info::DeviceInfo;

    fn nearby() -> DeviceView {
        let info = DeviceInfo::local("p".into(), "Pixel 9".into());
        let mut device = DeviceView::from_info(&info, "10.0.0.2".into(), false);
        device.status = DeviceStatus::Discovered;
        device.device_type = "phone".into();
        device
    }

    fn asked() -> DeviceView {
        let mut device = nearby();
        device.status = DeviceStatus::PairRequested;
        device
    }

    /// A phone we asked and that went back to "nearby" is shown as refused
    /// (so the page can say so and offer Try again), for a while, and not
    /// when it paired, or asked us, or was never asked.
    #[test]
    fn a_refused_request_stays_in_the_list_with_the_reason() {
        let t0 = std::time::Instant::now();
        let mut watch = PairWatch::default();
        let (kept, newly) = watch.observe(&[nearby()], t0);
        assert!(kept.is_empty() && newly.is_empty(), "never asked");

        assert!(watch.observe(&[asked()], t0).0.is_empty());
        let (kept, newly) = watch.observe(&[nearby()], t0);
        assert_eq!(newly, ["Pixel 9"]);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].status, DeviceStatus::Discovered);
        assert_eq!(
            kept[0].last_error.as_deref(),
            Some("Pairing request was rejected")
        );
        // The same look again says nothing new and keeps it.
        let (kept, newly) = watch.observe(&[nearby()], t0);
        assert!(newly.is_empty() && kept.len() == 1);
        // It also stays when the daemon has dropped it from its list.
        assert_eq!(watch.observe(&[], t0).0.len(), 1);
        // After two minutes it goes.
        let later = t0 + REFUSED_STAYS;
        assert!(watch.observe(&[], later).0.is_empty());
    }

    #[test]
    fn a_request_that_pairs_or_is_asked_again_is_not_refused() {
        let t0 = std::time::Instant::now();
        let mut watch = PairWatch::default();
        watch.observe(&[asked()], t0);
        let mut paired = asked();
        paired.trusted = true;
        paired.status = DeviceStatus::Paired;
        let (kept, newly) = watch.observe(&[paired], t0);
        assert!(kept.is_empty() && newly.is_empty());

        watch.observe(&[asked()], t0);
        watch.observe(&[nearby()], t0);
        // Asked again: the old refusal is gone.
        assert!(watch.observe(&[asked()], t0).0.is_empty());
        // The phone asking us is its own page, not a refusal.
        let mut theirs = nearby();
        theirs.status = DeviceStatus::PairRequestedByPeer;
        watch.observe(&[asked()], t0);
        assert!(watch.observe(&[theirs], t0).1.is_empty());
    }

    /// What the window lists, and what the page of a listed phone says: a
    /// refused phone is listed (a plain nearby one is not), and its page
    /// shows the "Couldn't pair" banner with Try again. (Needs a display.)
    #[test]
    fn a_refused_phone_is_listed_and_its_page_says_why() {
        assert!(!super::super::is_listed(&nearby(), false), "plain nearby");
        assert!(super::super::is_listed(&nearby(), true), "refused");
        assert!(super::super::is_listed(&asked(), false));
        crate::test_display::run(|| {
            let mut watch = PairWatch::default();
            let t0 = std::time::Instant::now();
            watch.observe(&[asked()], t0);
            let (kept, _) = watch.observe(&[nearby()], t0);
            let (daemon, _commands) = DaemonHandle::detached();
            let page = pair_idle_view(&daemon, &kept[0]);
            let mut texts = Vec::new();
            let mut stack = vec![page.upcast::<gtk::Widget>()];
            while let Some(widget) = stack.pop() {
                if let Some(label) = widget.downcast_ref::<gtk::Label>() {
                    texts.push(label.label().to_string());
                }
                if let Some(button) = widget.downcast_ref::<gtk::Button>() {
                    texts.extend(button.label().map(|text| text.to_string()));
                }
                let mut child = widget.first_child();
                while let Some(next) = child {
                    child = next.next_sibling();
                    stack.push(next);
                }
            }
            assert!(
                texts.iter().any(|t| t == "Couldn't pair with Pixel 9"),
                "{texts:?}"
            );
            assert!(texts.iter().any(|t| t == "Try again"), "{texts:?}");
        });
    }

    #[test]
    fn the_welcome_says_where_to_get_the_phone_app() {
        assert_eq!(INSTALL_ADDRESS, "github.com/derpx06/DeskLink/releases");
        assert!(WELCOME_HELP.contains("scan this code"));
    }

    /// A nearby phone's line says what it is and, once a request went
    /// wrong, why, in plain sentence-case words.
    #[test]
    fn a_nearby_phone_reads_as_its_kind_its_state_and_why_it_failed() {
        let mut device = nearby();
        assert_eq!(discovery_subtitle(&device), "Phone");
        device.status = DeviceStatus::PairRequestedByPeer;
        assert_eq!(discovery_subtitle(&device), "Phone · Wants to pair");
        device.status = DeviceStatus::PairRequested;
        assert_eq!(
            discovery_subtitle(&device),
            "Phone · Accept the request on the phone"
        );
        device.status = DeviceStatus::Discovered;
        device.last_error = Some("Pairing failed".into());
        assert_eq!(discovery_subtitle(&device), "Phone · Pairing failed");
        assert_eq!(PAIR_TITLE, "Pair another phone");
        assert_eq!(PAIR_CODE_TITLE, "Pair with a code");
    }
}
