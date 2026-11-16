//! Home's tiles: the column count by width, each feature tile with what it
//! needs, the More tile, and handing off to the phone.
//!
//! Sibling edges: `links` (the send-link dialog).

use super::*;

/// The gap between tiles, px.
const TILE_GAP: i32 = 12;

/// How many tiles a row of `width` px holds when each needs `tile` px: six,
/// three or two (6 divides into rows of 6, 3 and 2, so there is never an
/// orphan tile on a line of its own, like "More" alone under five).
pub(in crate::window) fn tile_columns(width: i32, tile: i32) -> u32 {
    [6, 3]
        .into_iter()
        .find(|&n| n * tile.max(1) + (n - 1) * TILE_GAP <= width)
        .unwrap_or(2) as u32
}

/// A control that cannot be used now: it looks disabled but stays a focus
/// stop, its hint (a tile's) is the short reason, its tooltip the need, and a
/// click says why with the fix. Nothing it would send is wired.
pub(super) fn unavailable(control: &gtk::Button, need: &Need) {
    control.add_css_class("dl-unavailable");
    control.set_tooltip_text(Some(&need.title));
    if let Some(hint) = named_widgets(control.upcast_ref(), "")
        .into_iter()
        .find(|w| w.has_css_class("dl-tile-hint"))
        .and_downcast::<gtk::Label>()
    {
        hint.set_label(crate::window::requirements::short(need));
    }
    control.update_property(&[gtk::accessible::Property::Description(&format!(
        "Unavailable. {}",
        need.title
    ))]);
    let need = need.clone();
    control.connect_clicked(move |control| crate::window::requirements::explain(control, &need));
}

/// The six quick actions: Phone screen, Insert, Sound, Ring phone, Hand off
/// and More. A row of six, three by two, or two by three, by the page's width.
pub(super) fn tiles(daemon: &DaemonHandle, device: &DeviceView) -> gtk::FlowBox {
    let kind = device_type_label(&device.device_type);
    let flow = gtk::FlowBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .homogeneous(true)
        .min_children_per_line(2)
        .max_children_per_line(3)
        .column_spacing(TILE_GAP as u32)
        .row_spacing(TILE_GAP as u32)
        .build();
    flow.set_widget_name("tile-row");
    let add = |tile: &gtk::Button| {
        tile.set_hexpand(true);
        flow.append(tile);
        if let Some(cell) = tile.parent() {
            cell.set_focusable(false);
        }
    };
    let phone = PhoneFacts::from_device(device);
    let blocked = |feature| blocker(feature, &phone);
    if desktop_sends(PACKET_TYPE_MOUSEPAD_REQUEST) {
        let need = blocked(Feature::Send(PACKET_TYPE_MOUSEPAD_REQUEST));
        let screen = tile(
            "desklink-mobile-screen-share-symbolic",
            &format!("{kind} screen"),
            Some("See and control it"),
            TileKind::Normal,
        );
        screen.set_tooltip_text(Some(&format!("Show {kind} Screen")));
        match &need {
            Some(need) => unavailable(&screen, need),
            None => {
                let (screen_daemon, id) = (daemon.clone(), device.id.clone());
                screen.connect_clicked(move |button| {
                    if let Some(root) = button.root().and_downcast::<gtk::Window>() {
                        crate::remote_control::show_remote_control_dialog(
                            screen_daemon.clone(),
                            id.clone(),
                            &root,
                        );
                    }
                });
            }
        }
        add(&screen);

        let insert = tile(
            "desklink-photo-camera-symbolic",
            "Insert",
            Some("Photo, scan, text"),
            TileKind::Normal,
        );
        insert.set_tooltip_text(Some(&format!(
            "Insert from {}: a photo, a scan, text or a QR code",
            device.name
        )));
        match &need {
            Some(need) => unavailable(&insert, need),
            None => crate::window::insert::attach_menu(daemon, device, &insert),
        }
        add(&insert);

        // Sound both ways (F1): its two switches in a popover.
        let on = device.sound.is_some();
        let sound = tile(
            "desklink-volume-up-symbolic",
            "Sound",
            Some(if on { "On" } else { "Phone to here" }),
            if on { TileKind::On } else { TileKind::Normal },
        );
        sound.set_tooltip_text(Some(&format!("Sound with {}", device.name)));
        match &need {
            Some(need) => unavailable(&sound, need),
            None => crate::window::sound::attach_menu(daemon, device, &sound),
        }
        add(&sound);
    }
    if desktop_sends(PACKET_TYPE_FINDMYPHONE_REQUEST) {
        let ring = tile(
            "desklink-ring-volume-symbolic",
            &format!("Ring {}", kind.to_lowercase()),
            Some("Find it"),
            TileKind::Normal,
        );
        ring.set_tooltip_text(Some(&format!("Ring {kind}")));
        match blocked(Feature::Send(PACKET_TYPE_FINDMYPHONE_REQUEST)) {
            Some(need) => unavailable(&ring, &need),
            None => {
                ring.connect_clicked(|button| {
                    if let Some(window) = button.root().and_downcast::<DesklinkWindow>() {
                        window.quick_action(QuickAction::Ring);
                    }
                });
            }
        }
        add(&ring);
    }
    // Walk away: lock here, carry on from the phone, in the newest terminal.
    let hand_off = tile(
        "desklink-lock-symbolic",
        "Hand off",
        Some("Continue on phone"),
        TileKind::Normal,
    );
    hand_off.set_tooltip_text(Some(&format!(
        "Lock this computer and open the newest terminal on {}",
        device.name
    )));
    match blocked(Feature::HandOff) {
        Some(need) => unavailable(&hand_off, &need),
        // Drawn as usable with or without an open terminal: with none, the
        // click offers to start one (no terminal is not a blocker).
        None => {
            let name = device.name.clone();
            hand_off.connect_clicked(move |button| {
                if let Some(window) = button.root().and_downcast::<DesklinkWindow>() {
                    self::hand_off(&window, &name);
                }
            });
        }
    }
    add(&hand_off);
    if let Some(more) = more_tile(daemon, device) {
        add(&more);
    }

    flow
}

/// "More actions for Pixel 9: Lock phone, Send a nudge": what is in it.
pub(super) fn more_label(name: &str, titles: &[&str]) -> String {
    format!("More actions for {name}: {}", titles.join(", "))
}

/// "More": Lock phone, Send a nudge, Send a link. A popover of rows, each an
/// icon square, a title and a hint. `None` when the phone takes none.
fn more_tile(daemon: &DaemonHandle, device: &DeviceView) -> Option<gtk::Button> {
    type Run = Box<dyn Fn(&gtk::Widget)>;
    let kind = device_type_label(&device.device_type);
    let phone = PhoneFacts::from_device(device);
    // Each row: icon, title, hint, what it needs, what it does.
    let mut items: Vec<(&str, String, &str, Option<Need>, Run)> = Vec::new();
    let need = |packet| blocker(Feature::Send(packet), &phone);
    if desktop_sends(PACKET_TYPE_LOCK_REQUEST) {
        items.push((
            "desklink-lock-symbolic",
            format!("Lock {}", kind.to_lowercase()),
            "Lock its screen",
            need(PACKET_TYPE_LOCK_REQUEST),
            Box::new(|anchor: &gtk::Widget| {
                if let Some(window) = anchor.root().and_downcast::<DesklinkWindow>() {
                    window.quick_action(QuickAction::Lock);
                }
            }),
        ));
    }
    if desktop_sends(PACKET_TYPE_PING) {
        items.push((
            "desklink-vibration-symbolic",
            "Send a nudge".to_string(),
            "A quiet notification",
            need(PACKET_TYPE_PING),
            Box::new(|anchor: &gtk::Widget| {
                if let Some(window) = anchor.root().and_downcast::<DesklinkWindow>() {
                    window.quick_action(QuickAction::Ping);
                }
            }),
        ));
    }
    if desktop_sends(PACKET_TYPE_SHARE_REQUEST) {
        let (daemon, id, name) = (daemon.clone(), device.id.clone(), device.name.clone());
        items.push((
            "desklink-link-symbolic",
            "Send a link…".to_string(),
            "Opens on the phone",
            need(PACKET_TYPE_SHARE_REQUEST),
            Box::new(move |anchor: &gtk::Widget| {
                show_send_link_dialog(daemon.clone(), id.clone(), &name, anchor)
            }),
        ));
    }
    if items.is_empty() {
        return None;
    }
    let more = tile(
        "desklink-more-vert-symbolic",
        "More",
        Some("Lock, nudge, link"),
        TileKind::Normal,
    );
    let titles: Vec<&str> = items
        .iter()
        .map(|(_, title, _, _, _)| title.as_str())
        .collect();
    more.update_property(&[gtk::accessible::Property::Label(&more_label(
        &device.name,
        &titles,
    ))]);
    // Not reachable: every row would say the same, so the tile says it.
    if let Some(reach) = need(PACKET_TYPE_PING).filter(|need| need.id == "reach") {
        unavailable(&more, &reach);
        return Some(more);
    }
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["dl-row-group"])
        .build();
    let runs: Vec<Result<Run, Need>> = items
        .into_iter()
        .map(|(icon, title, hint, need, run)| {
            let row = crate::window::ui_kit::list_row(
                icon,
                Tone::Neutral,
                &title,
                Some(need.as_ref().map_or(hint, |need| need.title.as_str())),
            );
            row.set_activatable(true);
            if need.is_some() {
                // Listed, looks unavailable; choosing it says why.
                row.add_css_class("dl-unavailable");
            }
            list.append(&row);
            need.map_or(Ok(run), Err)
        })
        .collect();
    let popover = gtk::Popover::builder()
        .child(&list)
        .css_classes(["more-popover"])
        .build();
    popover.set_parent(&more);
    more.connect_destroy({
        let popover = popover.clone();
        move |_| popover.unparent()
    });
    let weak = popover.downgrade();
    // Weakly: the tile owns the popover, which owns this closure.
    let anchor = more.downgrade();
    list.connect_row_activated(move |_, row| {
        if let Some(popover) = weak.upgrade() {
            popover.popdown();
        }
        let (Some(anchor), Some(run)) = (
            anchor.upgrade(),
            usize::try_from(row.index()).ok().and_then(|i| runs.get(i)),
        ) else {
            return;
        };
        match run {
            Ok(run) => run(anchor.upcast_ref()),
            Err(need) => crate::window::requirements::explain(&anchor, need),
        }
    });
    let shown = popover.downgrade();
    more.connect_clicked(move |_| {
        if let Some(popover) = shown.upgrade() {
            popover.popup();
        }
    });
    Some(more)
}

/// The small prompt Hand off shows with no open terminal: (heading, body,
/// button).
pub(super) fn no_terminal_words(phone: &str) -> (&'static str, String, &'static str) {
    (
        "No terminal to hand off",
        format!(
            "Start one now? DeskLink starts a shared terminal, then locks this computer and \
             opens it on {phone}."
        ),
        "Start a terminal",
    )
}

/// Hand off (the Home tile, the palette): lock here and carry on in the
/// newest terminal on the phone. With no open terminal, a small prompt
/// offers to start one, and then hands it off.
pub(in crate::window) fn hand_off(window: &DesklinkWindow, phone: &str) {
    if crate::device_links::pty::registry::newest_session().is_some() {
        gtk::prelude::WidgetExt::activate_action(window, "app.lock-hand-off", None).ok();
        return;
    }
    let (heading, body, start) = no_terminal_words(phone);
    let dialog = adw::AlertDialog::new(Some(heading), Some(&body));
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("start", start);
    dialog.set_response_appearance("start", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("start"));
    dialog.set_close_response("cancel");
    let weak = window.downgrade();
    dialog.connect_response(Some("start"), move |_, _| {
        let Some(window) = weak.upgrade() else {
            return;
        };
        match crate::device_links::pty::desk::new_desk_session() {
            // Registered before this returns, so the hand-off finds it.
            Ok(_) => {
                gtk::prelude::WidgetExt::activate_action(&window, "app.lock-hand-off", None).ok();
                window.refresh_soon();
            }
            Err(error) => {
                eprintln!("[UI] Could not start a terminal: {error}");
                window.show_feature_toast("Could not start a terminal");
            }
        }
    });
    dialog.present(Some(window));
}

/// Whether this build of DeskLink sends `packet` at all (a feature it has).
pub(super) fn desktop_sends(packet: &str) -> bool {
    crate::device_links::features::FeatureRegistry::desktop()
        .outgoing_capabilities()
        .iter()
        .any(|capability| capability == packet)
}
