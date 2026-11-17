//! Debug builds only: the `--debug-ui` verbs that drive the window for
//! screenshots and tests (fake devices, keys, screenshots, popovers).
//!
//! `debug_ui_verb` is one ~880-line match, one arm per verb: an accepted
//! exception to the file-size rule (splitting a match is a logic edit, and
//! it is debug-only).
//!
//! Sibling edges: `imp`, `blocks`, `navigation`/`refresh`/`notices` (window
//! methods).

use super::*;

/// Debug builds only: the `e2e` verbs that drive the UI itself, for
/// screenshots without a phone. Returns whether `verb` was one of them.
///
/// - `screenshot PATH`: the main window to PATH, every other visible window
///   beside it as `<stem>-<title>.png`.
/// - `fake-device KIND[:TYPE] NAME`: shows a made-up device (`paired`,
///   `paired-turn`, `paired-new` (no terminal access, no notification access),
///   `connecting`, `unreachable`, `pair-request`, `requested`,
///   `discovered`, `refused`; TYPE `phone`, `tablet`, `tv`, `laptop`, `desktop`);
///   `fake-clear` removes them.
/// - `ui-kit [W H]`: the UI kit's component sheet in its own window.
/// - `page NAME`, `select N` (the Nth fake device), `open-terminal N`,
///   `open-switcher`, `window-size W H`, `color-scheme dark|light|default`,
///   `open-pair`, `open-codes`, `nav content|sidebar`,
///   `fake-transfer NAME DONE/TOTAL` (a leading `-` on DONE makes it
///   incoming; TOTAL 0 is still preparing).
#[cfg(debug_assertions)]
pub(super) fn debug_ui_verb(win: &DesklinkWindow, verb: &str, id: &str, arg: &str) -> bool {
    match verb {
        "screenshot" => debug_screenshot(win, id),
        "fake-device" => {
            let device = debug_fake_device(id, arg);
            // Shown as already seen: no toasts in the screenshots.
            for notification in &device.notifications {
                win.imp()
                    .notified_remote_notifications
                    .borrow_mut()
                    .mark_seen(format!("{}:{}", device.id, notification.id), notification);
            }
            win.imp().fake_devices.borrow_mut().push(device);
            win.imp().snapshots.borrow_mut().clear();
            win.refresh_devices();
        }
        // "fake-phone-report KEY=on|off,...": the selected phone's Ask
        // report (`asks`), as if it had sent one; "fake-ask KEY": the
        // selected phone asks this computer for KEY (Ask on computer).
        "fake-phone-report" | "fake-ask" => {
            use crate::device_links::packet::{
                NetworkPacket, PACKET_TYPE_SETTINGS, PACKET_TYPE_SETTINGS_REQUEST,
            };
            let Some(device) = win.selected_id() else {
                return true;
            };
            if verb == "fake-ask" {
                let mut packet = NetworkPacket::new(PACKET_TYPE_SETTINGS_REQUEST);
                packet.set("set", serde_json::json!({ id: true }));
                crate::device_links::asks::on_phone_request(&device, &packet);
            } else {
                let features: serde_json::Map<String, serde_json::Value> = id
                    .split(',')
                    .filter_map(|pair| pair.split_once('='))
                    .map(|(key, on)| (key.to_string(), (on == "on").into()))
                    .collect();
                let mut packet = NetworkPacket::new(PACKET_TYPE_SETTINGS);
                packet.set("features", serde_json::Value::Object(features));
                packet.set("prompts", if arg == "blocked" { "off" } else { "on" });
                crate::device_links::asks::on_phone_report(&device, &packet);
            }
            win.imp().snapshots.borrow_mut().clear();
            win.refresh_devices();
        }
        "fake-clear" => {
            win.imp().fake_devices.borrow_mut().clear();
            win.imp().snapshots.borrow_mut().clear();
            win.refresh_devices();
        }
        "select" => {
            let device = id
                .parse::<usize>()
                .ok()
                .and_then(|index| win.imp().fake_devices.borrow().get(index).cloned());
            if let Some(device) = device {
                win.select_device(&device.id);
            }
        }
        "page" => {
            win.imp().device_card.popdown();
            win.show_page(id);
        }
        "open-terminal" => {
            let sessions = crate::terminal_previews::sessions();
            if let Some(info) = id.parse::<usize>().ok().and_then(|n| sessions.get(n)) {
                if let Some(page) = win.imp().terminals.borrow().as_ref() {
                    page.open_detail(win, &info.id);
                }
            }
        }
        // "terminal-type WORDS": typed into the open detail's terminal, then Enter.
        #[cfg(feature = "vte")]
        "terminal-type" => {
            if let Some(terminal) = win
                .imp()
                .terminals
                .borrow()
                .as_ref()
                .and_then(|page| page.live_terminal())
            {
                terminal.type_text(&format!("{}\r", format!("{id} {arg}").trim()));
            }
        }
        // "xkeys ctrl+k text:echo_hi Return": real X key presses (XTest),
        // through GTK's whole key path, on the QA display only.
        "xkeys" => {
            let spec = format!("{id} {arg}");
            std::thread::spawn(move || debug_xkeys(&spec));
        }
        // "click-unavailable N": clicks the Nth unavailable control on show
        // (it says why, with the fix).
        "click-unavailable" => {
            let n: usize = id.parse().unwrap_or(0);
            let found: Vec<gtk::Button> = named_widgets(win.upcast_ref(), "")
                .into_iter()
                .filter(|w| w.has_css_class("dl-unavailable") && w.is_mapped())
                .filter_map(|w| w.downcast::<gtk::Button>().ok())
                .collect();
            if let Some(button) = found.into_iter().rev().nth(n) {
                button.emit_clicked();
            }
        }
        // "onboarding start [STEP]|next|back|skip|key KEYNAME": the welcome tour.
        "onboarding" => match id {
            "start" => {
                win.start_onboarding(arg.parse().unwrap_or(0));
                win.refresh_devices();
            }
            _ => {
                let tour = win.imp().onboarding.borrow().clone();
                if let Some(tour) = tour {
                    let nav = match id {
                        "next" => Some(onboarding::Nav::Next),
                        "back" => Some(onboarding::Nav::Back),
                        "skip" => Some(onboarding::Nav::Skip),
                        "key" => gtk::gdk::Key::from_name(arg)
                            .and_then(|key| onboarding::key_nav(key, false)),
                        _ => None,
                    };
                    if let Some(nav) = nav {
                        tour.apply(nav);
                    }
                }
            }
        },
        // "onboarding-gnome off|login|active|none": the top-bar state the
        // setup step shows; "real" goes back to this desktop's.
        "onboarding-gnome" => {
            use crate::shell_dbus::ExtensionStatus;
            let fake = match id {
                "off" => Some(Some(ExtensionStatus::Off)),
                "login" => Some(Some(ExtensionStatus::NeedsLogin)),
                "active" => Some(Some(ExtensionStatus::Active)),
                "none" => Some(None),
                _ => None,
            };
            onboarding::DEBUG_EXTENSION.with(|slot| slot.replace(fake));
        }
        "open-switcher" => win.imp().device_card.popup(),
        // "fake-shell-device KIND NAME": a made-up device for the GNOME
        // integration (extension, media, search), not this window.
        "fake-shell-device" => {
            let mut device = debug_fake_device(id, arg);
            if id == "live" {
                device.media_status = Some(crate::device_links::device::MediaStatus {
                    player: "Spotify".into(),
                    player_list: vec!["Spotify".into()],
                    title: "Blurred".into(),
                    artist: "Kiasmos".into(),
                    album: "Blurred EP".into(),
                    is_playing: true,
                    volume: Some(60),
                    position: Some(72_000),
                    length: Some(281_000),
                    can_seek: true,
                    position_at: None,
                    seek_hold: None,
                    album_art_url: None,
                    album_art: None,
                });
            }
            if id.starts_with("unreachable") {
                device.last_seen = Some(crate::device_links::connection_log::now() - 3600);
            }
            if let Ok(mut fake) = crate::device_links::daemon::handle::DEBUG_SHELL_DEVICES.lock() {
                fake.push(device);
            }
        }
        // "scroll-end PAGE": that page scrolled to its end.
        // "scroll-to PAGE Y": that page scrolled to Y pixels.
        "scroll-to" => {
            if let Some((scroll, _)) = win.imp().bodies.borrow().get(id).cloned() {
                // A number: that offset; else a widget name: its top, in view.
                let top = arg.parse().ok().or_else(|| {
                    let child = scroll.child()?;
                    let widget = named_widgets(child.upcast_ref(), arg).into_iter().next()?;
                    Some(f64::from(widget.compute_bounds(&child)?.y()) - 12.0)
                });
                scroll.vadjustment().set_value(top.unwrap_or(0.0));
            }
        }
        // "where PREFIX": the centre of each shown widget named PREFIX...,
        // in the window surface's pixels (for real clicks with xclick.py).
        "where" => {
            let (dx, dy) = gtk::prelude::NativeExt::surface_transform(win);
            for widget in named_widgets(win.upcast_ref(), id) {
                if let Some(at) = widget
                    .is_mapped()
                    .then(|| widget.compute_bounds(win))
                    .flatten()
                {
                    debug_log!(
                        "[E2E] where {} {} {}",
                        widget.widget_name(),
                        (dx + f64::from(at.x() + at.width() / 2.0)) as i32,
                        (dy + f64::from(at.y() + at.height() / 2.0)) as i32
                    );
                }
            }
        }
        // "settings-page list|N|search [words]": walks Settings' subpages.
        "settings-page" => prefs::debug_open(id, arg),
        // Pages with their own scrollers: every one on screen.
        "scroll-end" => {
            let page = win.imp().page_stack.child_by_name(id);
            let scrollers: Vec<gtk::Widget> = match win.imp().bodies.borrow().get(id) {
                Some((scroll, _)) => vec![scroll.clone().upcast()],
                None => page
                    .map(|page| named_widgets(&page, ""))
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|widget| widget.is::<gtk::ScrolledWindow>() && widget.is_mapped())
                    .collect(),
            };
            for scroll in scrollers
                .iter()
                .filter_map(|w| w.downcast_ref::<gtk::ScrolledWindow>())
            {
                let adjustment = scroll.vadjustment();
                adjustment.set_value(adjustment.upper());
                // A child laid out shorter than it asks for is cut at the bottom.
                if let Some(child) = scroll.child().and_then(|viewport| viewport.first_child()) {
                    let (min, nat, _, _) = child.measure(gtk::Orientation::Vertical, child.width());
                    debug_log!(
                        "[E2E] scroll-end {id}: upper {} page {} value {}, child {} high, min {min}, natural {nat}",
                        adjustment.upper(),
                        adjustment.page_size(),
                        adjustment.value(),
                        child.height()
                    );
                }
            }
        }
        // New Terminal's menu, with its "Start in" folders.
        "open-new-terminal" => {
            if let Some(page) = win.imp().terminals.borrow().as_ref() {
                for widget in named_widgets(&page.root, "new-terminal") {
                    if let Some(button) = widget.downcast_ref::<adw::SplitButton>() {
                        button.popup();
                    }
                }
            }
        }
        // "open-reply ID": notification ID's reply entry.
        // "focus-note N": the Nth notification card has the keyboard focus.
        "focus-note" => {
            if let Some(page) = win.imp().notifications.borrow().as_ref() {
                page.focus_card(id.parse().unwrap_or(0));
            }
            // As after a key press: the ring shows.
            gtk::prelude::GtkWindowExt::set_focus_visible(win, true);
        }
        // "fake-notification APP TITLE [reply]": one more notification on the
        // selected fake phone (underscores for spaces), newest first.
        "fake-notification" => {
            let selected = win.selected_id().unwrap_or_default();
            let mut fakes = win.imp().fake_devices.borrow_mut();
            if let Some(device) = fakes.iter_mut().find(|device| device.id == selected) {
                let (title, reply) = match arg.strip_suffix(" reply") {
                    Some(title) => (title, true),
                    None => (arg, false),
                };
                let n = device.notifications.len() + 100;
                device.notifications.insert(
                    0,
                    crate::device_links::device::DeviceNotification {
                        id: n.to_string(),
                        app_name: id.replace('_', " "),
                        title: title.replace('_', " "),
                        text: "Sent just now".into(),
                        ticker: String::new(),
                        is_clearable: true,
                        request_reply_id: reply.then(|| format!("r{n}")),
                        actions: Vec::new(),
                        silent: false,
                        time: gtk::glib::real_time() / 1000,
                        app_icon: None,
                    },
                );
            }
            drop(fakes);
            win.imp().snapshots.borrow_mut().clear();
            win.refresh_devices();
        }
        // "notif-filter N": the Notifications page's Nth filter chip (0 All).
        "notif-filter" => {
            if let Some(chips) = named_widgets(win.upcast_ref(), "notification-filter").first() {
                let mut child = chips.first_child();
                let mut n: usize = id.parse().unwrap_or(0);
                while let Some(chip) = child {
                    if n == 0 {
                        if let Some(chip) = chip.downcast_ref::<gtk::ToggleButton>() {
                            chip.set_active(true);
                        }
                        break;
                    }
                    n -= 1;
                    child = chip.next_sibling();
                }
            }
        }
        "open-reply" => {
            if let Some(page) = win.imp().notifications.borrow().as_ref() {
                page.open_reply(id);
            }
        }
        // A shell session at the desk without opening a window for it.
        "spawn-terminal" => {
            let created = crate::device_links::pty::registry::registry()
                .lock()
                .map_err(|_| "registry lock".to_string())
                .and_then(|mut registry| registry.create(None, 24, 80, "desk"));
            debug_log!("[E2E] spawn-terminal: {:?}", created.map(|info| info.id));
        }
        // "fake-attach N": the selected device attached to terminal N.
        "fake-attach" => {
            let sessions = crate::terminal_previews::sessions();
            if let (Some(info), Some(device)) = (
                id.parse::<usize>().ok().and_then(|n| sessions.get(n)),
                win.selected_id(),
            ) {
                if let Ok(mut fake) =
                    crate::device_links::webrtc::terminal_bridge::DEBUG_ATTACHED.lock()
                {
                    fake.push((info.id.clone(), device));
                }
            }
        }
        // "desk-note-close ID": the phone dismissing this computer's
        // notification ID (C1, the real path).
        "desk-note-close" => crate::device_links::desk_notifications::close_from_phone(id),
        // "clip-image-in PATH": a phone's copied PNG arriving (the real path).
        "clip-image-in" => crate::device_links::daemon::clipboard::debug_receive_image(id),
        // "clip-in TEXT": a phone's clipboard arriving (the real path).
        "clip-in" => {
            crate::device_links::daemon::clipboard::debug_receive(format!("{id} {arg}").trim());
        }
        // "fake-clip TEXT" / "fake-clip-phone TEXT": a clipboard history item.
        "fake-clip" | "fake-clip-phone" => {
            let text = format!("{id} {arg}");
            crate::device_links::daemon::clipboard::debug_remember(
                text.trim(),
                (verb == "fake-clip-phone").then_some("Pixel 9"),
            );
        }
        // These two write history files: only into an isolated runner's own
        // state directory, never the user's.
        "fake-history" | "fake-received" | "fake-sent"
            if std::env::var_os("XDG_STATE_HOME").is_none() =>
        {
            debug_log!("[E2E] {verb} refused: set XDG_STATE_HOME to an isolated directory");
        }
        // "fake-history N": a few connection-history entries for fake device N.
        // "fake-contacts N": N shared contacts for the selected device, as
        // if its phone had sent them (some with photos).
        "fake-contacts" => {
            use crate::device_links::packet::NetworkPacket;
            let Some(device) = win.selected_id() else {
                return true;
            };
            let count: usize = id.parse().unwrap_or(40);
            let first = [
                "Mara", "Bo", "Åsa", "Lars", "Chen", "Priya", "Omar", "Zoe", "Ines", "Kai", "Noor",
                "Femi", "Yuki", "Diego", "Hana",
            ];
            let last = [
                "Lindqvist",
                "Okafor",
                "Sato",
                "Nguyen",
                "Berg",
                "Silva",
                "Kaur",
            ];
            let photo =
                gtk::gdk_pixbuf::Pixbuf::new(gtk::gdk_pixbuf::Colorspace::Rgb, false, 8, 96, 96)
                    .and_then(|pixbuf| {
                        pixbuf.fill(0x3d7a_c4ff);
                        pixbuf.save_to_bufferv("jpeg", &[]).ok()
                    })
                    .map(|bytes| {
                        base64::Engine::encode(&base64::engine::general_purpose::STANDARD, bytes)
                    });
            let uids: Vec<String> = (0..count).map(|n| format!("lookup-{n}")).collect();
            let mut snapshot = NetworkPacket::new("desklink.contacts.response_uids_timestamps");
            snapshot.set("uids", uids.clone());
            for uid in &uids {
                snapshot.set(uid, "1");
            }
            snapshot.set("sharing", true);
            let _ = crate::device_links::contacts::on_snapshot(&device, None, &snapshot);
            let mut cards = NetworkPacket::new("desklink.contacts.response_vcards");
            cards.set("uids", uids.clone());
            for (n, uid) in uids.iter().enumerate() {
                let mut card = format!(
                    "BEGIN:VCARD\nVERSION:3.0\nFN:{} {}\nTEL;TYPE=CELL:+46 70 {:03} {:02} {:02}\n",
                    first[n % first.len()],
                    last[n % last.len()],
                    n,
                    n % 100,
                    (n * 7) % 100
                );
                if n % 3 == 0 {
                    card.push_str("TEL;TYPE=WORK:08-123 456\nEMAIL:someone@example.org\nORG:Acme\nNOTE:Met at the conference\\nlikes tea\n");
                }
                if let (0, Some(photo)) = (n % 4, &photo) {
                    card.push_str(&format!("PHOTO;ENCODING=b;TYPE=JPEG:{photo}\n"));
                }
                card.push_str("END:VCARD");
                cards.set(uid, card);
            }
            crate::device_links::contacts::on_vcards(&device, &cards);
            win.refresh_soon();
        }
        // "fake-snippet TEXT" (underscores for spaces) and "fake-note": F6
        // content without a phone.
        // F8b: "fake-bucket": a Bucket with items in every state for the
        // selected device (shots without a phone); "bucket-on" turns the real
        // one on; "bucket-add PATH" adds a file or folder as a drop would.
        "fake-bucket" => {
            use crate::device_links::folders::{
                self, Item, ItemState, Pair, PairView, Phase, Side,
            };
            let Some(device) = win.selected_id() else {
                return true;
            };
            let root = std::env::var_os("DESKLINK_FAKE_BUCKET")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| glib::home_dir().join("DeskLink Bucket"));
            let item = |path: &str, size: u64, ago: i64, from: Side, state: ItemState| Item {
                dir: false,
                path: path.into(),
                size,
                mtime: 1_761_130_000_000 - ago * 60_000,
                from,
                state,
            };
            let folder = |path: &str, ago: i64| Item {
                dir: true,
                ..item(path, 0, ago, Side::Desk, ItemState::Synced)
            };
            let items = vec![
                folder("Trips", 2),
                folder("Trips/Rome", 3),
                folder("Receipts", 500),
                item(
                    "Trips/Rome/Video.mov",
                    600_000_000,
                    2,
                    Side::Phone,
                    ItemState::Receiving,
                ),
                item(
                    "Trips/Rome/Ticket.pdf",
                    90_000,
                    3,
                    Side::Phone,
                    ItemState::Synced,
                ),
                item("Trips/Map.jpg", 2_400_000, 3, Side::Desk, ItemState::Synced),
                item(
                    "Boarding pass.pdf",
                    182_000,
                    1,
                    Side::Phone,
                    ItemState::Receiving,
                ),
                item(
                    "Sunset at the pier.jpg",
                    3_400_000,
                    4,
                    Side::Phone,
                    ItemState::Synced,
                ),
                item(
                    "Quarterly report.pdf",
                    920_000,
                    15,
                    Side::Desk,
                    ItemState::Sending,
                ),
                item("notes.md", 4_200, 40, Side::Desk, ItemState::Conflict),
                item("Wi-Fi password.txt", 64, 90, Side::Phone, ItemState::Synced),
                item(
                    "github.com 2026-10-22.url",
                    90,
                    120,
                    Side::Desk,
                    ItemState::Waiting,
                ),
                item(
                    "Invoice 2041.pdf",
                    210_000,
                    300,
                    Side::Desk,
                    ItemState::Synced,
                ),
            ];
            let bytes = items.iter().map(|item| item.size).sum();
            let items: Vec<Item> = if id == "empty" { Vec::new() } else { items };
            folders::debug_show(
                vec![PairView {
                    pair: Pair {
                        id: folders::BUCKET_ID.into(),
                        name: "DeskLink Bucket".into(),
                        root,
                        device: device.clone(),
                        include_hidden: false,
                        ignore: Vec::new(),
                        bucket: true,
                    },
                    phase: Phase::UpToDate,
                    files: items.len(),
                    last_sync: Some(glib::real_time() / 1000 - 120_000),
                    conflicts: vec![folders::Conflict {
                        path: "notes.md".into(),
                        copy: "notes (conflict from Pixel 9 2026-10-22).md".into(),
                        at: 0,
                    }],
                    items,
                    bytes,
                    skipped: if id == "empty" {
                        Vec::new()
                    } else {
                        vec!["Old photos".into()]
                    },
                }],
                Vec::new(),
            );
            win.refresh_soon();
        }
        // "fake-bucket-progress [paused]": the fake Bucket's moving items
        // as slow transfers (about 8 MB/s each), for progress shots.
        "fake-bucket-progress" => {
            use crate::device_links::webrtc::transfer_board::{self, TransferRow};
            let Some(device) = win.selected_id() else {
                return true;
            };
            let rows = [
                ("fake-in", "Boarding pass.pdf", true, 1_200_000_000u64),
                ("fake-in2", "Trips/Rome/Video.mov", true, 600_000_000),
                ("fake-out", "Quarterly report.pdf", false, 900_000_000),
            ];
            for (id, name, incoming, total) in rows {
                transfer_board::begin_synced(TransferRow {
                    id: id.into(),
                    device_id: device.clone(),
                    name: name.into(),
                    incoming,
                    done: if incoming { 340_000_000 } else { 0 },
                    total,
                    preparing: false,
                    paused: false,
                });
            }
            if id == "paused" {
                transfer_board::pause_device(&device);
                return true;
            }
            let started = std::time::Instant::now();
            glib::timeout_add_local(std::time::Duration::from_millis(250), move || {
                let moved = (started.elapsed().as_secs_f64() * 8e6) as u64;
                transfer_board::progress("fake-in", 340_000_000 + moved);
                // The second file starts after a second ("Waiting…" first).
                transfer_board::progress("fake-out", moved.saturating_sub(8_000_000));
                glib::ControlFlow::Continue
            });
        }
        // "bucket-open Trips/Rome": the Bucket page inside that folder ("top"
        // for the top); "bucket-select a|b": those items, selecting.
        "bucket-open" => bucket::debug_open(win, if id == "top" { "" } else { id }),
        "bucket-select" => bucket::debug_select(id.split('|').map(str::to_string).collect()),
        "bucket-on" => {
            if let Some(device) = win.selected_id() {
                crate::device_links::folders::turn_on_bucket(&device);
            }
        }
        "bucket-add" => {
            if let Some(device) = win.selected_id() {
                debug_log!("[E2E] bucket-add");
                crate::device_links::folders::import(
                    &device,
                    vec![std::path::PathBuf::from(id)],
                    arg,
                );
            }
        }
        "fake-snippet" => {
            let _ = crate::device_links::notes::add_snippet(&id.replace('_', " "));
            win.refresh_soon();
        }
        "fake-note" => {
            let verb_id = id;
            // "fake-note Some_words|second_line": that text (shots of a list).
            if !id.is_empty() && id != "pinned" {
                let _ =
                    crate::device_links::notes::new_note(&id.replace('_', " ").replace('|', "\n"));
                win.refresh_soon();
                return true;
            }
            let id = crate::device_links::notes::new_note(
                "# Release checklist\nShip **F6** after the *real-phone* run.\n\n\
                 - [x] contract\n- [ ] Android side\n- `cargo test` green\n1. tag\n2. push",
            );
            if let (Some(note), "pinned") = (id, verb_id) {
                let _ = crate::device_links::notes::edit_note(&note, |note| note.pinned = true);
            }
            win.refresh_soon();
        }
        // "open-note N": the Nth note in the list, as if clicked.
        "open-note" => {
            let solo = win.imp().main_stack.visible_child_name().as_deref() == Some("solo");
            if solo {
                if let Some(solo) = win.imp().solo.borrow().as_ref() {
                    solo.notes.open_nth(id.parse().unwrap_or(0));
                }
            } else if let Some(page) = win.imp().notes.borrow().as_ref() {
                page.notes.open_nth(id.parse().unwrap_or(0));
            }
        }
        // "open-contact UID": the selected device's contact, as from search.
        "open-contact" => {
            if let Some(device) = win.selected_id() {
                win.show_contact(&device, id);
            }
        }
        "fake-history" => {
            use crate::device_links::connection_log::{
                record_with_path, remember_name, ConnectionEvent,
            };
            let device = id
                .parse::<usize>()
                .ok()
                .and_then(|index| win.imp().fake_devices.borrow().get(index).cloned());
            if let Some(device) = device {
                remember_name(&device.id, &device.name);
                record_with_path(&device.id, ConnectionEvent::Connected, Some("turn"));
                record_with_path(&device.id, ConnectionEvent::TerminalAttached, None);
                record_with_path(&device.id, ConnectionEvent::ScreenViewStarted, None);
                record_with_path(&device.id, ConnectionEvent::Connected, Some("lan"));
            }
        }
        // "fake-received N NAME": a file received from fake device N.
        // "fake-received N PATH" / "fake-sent N PATH": a file in the
        // history of fake device N (PATH absolute, or a name in /tmp).
        "fake-received" | "fake-sent" => {
            let device = id
                .parse::<usize>()
                .ok()
                .and_then(|index| win.imp().fake_devices.borrow().get(index).cloned());
            let path = std::path::Path::new(arg);
            let path = if path.is_absolute() {
                path.to_path_buf()
            } else {
                std::env::temp_dir().join(arg)
            };
            let name = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            if let Some(device) = device {
                if verb == "fake-sent" {
                    crate::device_links::transfer_log::record_sent(
                        &device.id,
                        &name,
                        &path,
                        std::fs::metadata(&path).map_or(0, |meta| meta.len()),
                    );
                } else {
                    crate::device_links::transfer_log::record_received(&device.id, &name, &path);
                }
            }
        }
        // "click TEXT": clicks the visible button whose tooltip starts with
        // TEXT (underscores for spaces), e.g. to open the Camera popover.
        "click" => {
            let wanted = id.replace('_', " ");
            let mut stack = vec![win.clone().upcast::<gtk::Widget>()];
            while let Some(widget) = stack.pop() {
                if let Some(button) = widget.downcast_ref::<gtk::Button>() {
                    if button.is_mapped()
                        && button
                            .tooltip_text()
                            .is_some_and(|tip| tip.starts_with(&wanted))
                    {
                        button.emit_clicked();
                        break;
                    }
                }
                let mut child = widget.first_child();
                while let Some(next) = child {
                    child = next.next_sibling();
                    stack.push(next);
                }
            }
        }
        "close-dialog" => {
            if let Some(dialog) = win.visible_dialog() {
                dialog.close();
            }
            let mut stack = vec![win.clone().upcast::<gtk::Widget>()];
            while let Some(widget) = stack.pop() {
                if let Some(popover) = widget.downcast_ref::<gtk::Popover>() {
                    popover.popdown();
                }
                let mut child = widget.first_child();
                while let Some(next) = child {
                    child = next.next_sibling();
                    stack.push(next);
                }
            }
        }
        // "open-viewer N": the selected device's Nth file still on disk.
        "open-viewer" => {
            let selected = win.selected_id().unwrap_or_default();
            let files: Vec<_> = crate::device_links::transfer_log::recent()
                .into_iter()
                .filter(|file| file.device_id == selected && file.path.is_file())
                .collect();
            if let Some(index) = id.parse::<usize>().ok().filter(|n| *n < files.len()) {
                viewer::show(win, &files, index, "Pixel 9");
            }
        }
        // "too-wide N [window]": the widgets on the visible page (or the
        // whole window) whose minimum width is over N px, deepest first (the
        // ones to fix for a narrow window).
        "too-wide" => {
            let limit: i32 = id.parse().unwrap_or(340);
            let mut found = Vec::new();
            let root: gtk::Widget = if arg == "window" {
                win.imp().toast_overlay.get().upcast()
            } else {
                win.imp().page_stack.get().upcast()
            };
            let mut stack = vec![(root, 0)];
            while let Some((widget, depth)) = stack.pop() {
                let (min, _, _, _) = widget.measure(gtk::Orientation::Horizontal, -1);
                if min > limit && widget.is_visible() && widget.is_mapped() {
                    let wide_child = {
                        let mut child = widget.first_child();
                        let mut any = false;
                        while let Some(next) = child {
                            if next.is_mapped()
                                && next.measure(gtk::Orientation::Horizontal, -1).0 > limit
                            {
                                any = true;
                            }
                            child = next.next_sibling();
                        }
                        any
                    };
                    if !wide_child {
                        found.push(format!(
                            "{min}px {} {:?} {:?}",
                            widget.type_().name(),
                            widget.widget_name(),
                            widget.css_classes()
                        ));
                    }
                }
                let mut child = widget.first_child();
                while let Some(next) = child {
                    stack.push((next.clone(), depth + 1));
                    child = next.next_sibling();
                }
            }
            debug_log!("[E2E] too-wide {limit}: {found:#?}");
        }
        // "ui-kit [W H]": the component sheet in its own window.
        "ui-kit" => {
            let size = |text: &str, default| text.parse().unwrap_or(default);
            ui_kit::present_sheet(win, size(id, 1100), size(arg, 1500));
        }
        "window-size" => {
            if let (Ok(width), Ok(height)) = (id.parse(), arg.parse()) {
                win.unmaximize();
                win.set_default_size(width, height);
            }
        }
        "color-scheme" => adw::StyleManager::default().set_color_scheme(match id {
            "dark" => adw::ColorScheme::ForceDark,
            "light" => adw::ColorScheme::ForceLight,
            _ => adw::ColorScheme::Default,
        }),
        "open-pair" => win.show_discovery_dialog(true),
        // "guide [scroll N|end]": the Get started guide, open or scrolled.
        "guide" if id == "scroll" => {
            if let Some(dialog) = win.visible_dialog() {
                for scroll in named_widgets(dialog.upcast_ref(), "")
                    .iter()
                    .filter_map(|w| w.downcast_ref::<gtk::ScrolledWindow>())
                {
                    let adjustment = scroll.vadjustment();
                    adjustment.set_value(arg.parse().unwrap_or(adjustment.upper()));
                }
            }
        }
        "guide" => guide::show(win),
        // "palette [QUERY]": "Find anything", with the words typed
        // (underscores for spaces); again closes it.
        "palette" => palette::toggle(win, &format!("{id} {arg}").trim().replace('_', " ")),
        "system-check" => system_check::show_dialog(win),
        // "fake-sticky": the phone link's parts missing, as the banner shows it.
        "fake-sticky" => {
            crate::platform::fake_missing_link();
            win.assert_sticky_notice();
        }
        // "fake-error TEXT": a daemon error, as the banner shows it.
        "fake-error" => win.report_error(&format!("{id} {arg}").trim().replace('_', " "), false),
        "open-codes" => win.show_discovery_dialog(false),
        // "fake-transfer NAME DONE/TOTAL": a transfer row.
        "fake-transfer" => {
            let (done, total) = arg.split_once('/').unwrap_or(("0", "0"));
            transfers::FAKE_ROWS.with(|rows| {
                rows.borrow_mut()
                    .push(crate::device_links::webrtc::transfer_board::TransferRow {
                        id: format!("fake-{}", glib::monotonic_time()),
                        device_id: String::new(),
                        name: id.to_string(),
                        incoming: done.starts_with('-'),
                        done: done.trim_start_matches('-').parse().unwrap_or(0),
                        total: total.parse().unwrap_or(0),
                        preparing: total == "0",
                        paused: false,
                    })
            });
        }
        "size-info" => {
            let imp = win.imp();
            debug_log!(
                "[E2E] window {}x{}, content {}x{}, page stack {}x{}",
                win.width(),
                win.height(),
                imp.content_page.width(),
                imp.content_page.height(),
                imp.page_stack.width(),
                imp.page_stack.height()
            );
            if let Some((scroll, body)) = imp.bodies.borrow().get(id).cloned() {
                let adjustment = scroll.vadjustment();
                debug_log!(
                    "[E2E] {id}: scroll upper {} page {} value {}, slot {}x{}",
                    adjustment.upper(),
                    adjustment.page_size(),
                    adjustment.value(),
                    body.width(),
                    body.height()
                );
            }
            for widget in named_widgets(&imp.page_stack.get().upcast(), "hero-") {
                let (min, nat, _, _) = widget.measure(gtk::Orientation::Horizontal, -1);
                debug_log!(
                    "[E2E] {}: {} wide, min {min}, natural {nat}",
                    widget.widget_name(),
                    widget.width()
                );
            }
        }
        // "a11y": every button, switch and menu button on the shown page with
        // no accessible name (its label, tooltip or an explicit one).
        "a11y" => {
            let mut bad = 0;
            let mut stack = vec![win.clone().upcast::<gtk::Widget>()];
            while let Some(widget) = stack.pop() {
                let interactive = widget.is::<gtk::Button>()
                    || widget.is::<gtk::MenuButton>()
                    || widget.is::<gtk::Switch>()
                    || widget.is::<gtk::ToggleButton>();
                if interactive && widget.is_mapped() && widget.is_visible() {
                    let named = widget
                        .downcast_ref::<gtk::Button>()
                        .map(|b| b.label().is_some_and(|l| !l.is_empty()))
                        .unwrap_or(false)
                        || widget.tooltip_text().is_some_and(|t| !t.is_empty())
                        || widget.has_css_class("has-label")
                        || widget.is::<gtk::Switch>()
                            && widget
                                .parent()
                                .is_some_and(|p| p.is::<gtk::Box>() && p.parent().is_some());
                    let labelled = has_label_descendant(&widget)
                        || widget.type_().name() == "AdwViewSwitcherButton"
                        || widget
                            .ancestor(gtk::WindowControls::static_type())
                            .is_some()
                        || widget.ancestor(gtk::Calendar::static_type()).is_some()
                        // The inner toggle of a menu button is named by it.
                        || widget.parent().is_some_and(|p| {
                            p.is::<gtk::MenuButton>() || p.type_().name() == "AdwSplitButton"
                        });
                    if !named && !labelled {
                        bad += 1;
                        debug_log!(
                            "[E2E] a11y: unnamed {} {:?} in {}",
                            widget.type_().name(),
                            widget.css_classes(),
                            widget
                                .parent()
                                .and_then(|p| p.parent())
                                .map(|p| p.type_().name().to_string())
                                .unwrap_or_default()
                        );
                    }
                }
                let mut child = widget.first_child();
                while let Some(next) = child {
                    child = next.next_sibling();
                    stack.push(next);
                }
            }
            debug_log!("[E2E] a11y: {bad} unnamed");
        }
        "focus-info" => {
            let focus = gtk::prelude::RootExt::focus(win);
            debug_log!(
                "[E2E] focus: {:?} {:?}",
                focus.as_ref().map(|widget| widget.type_().name()),
                focus.as_ref().map(|widget| widget.widget_name())
            );
            if let Some(text) = focus.and_then(|widget| widget.downcast::<gtk::Text>().ok()) {
                // Its length only: the focused field may be a password.
                debug_log!(
                    "[E2E] focus text of {} chars, cursor {}, selection {:?}",
                    text.text().chars().count(),
                    text.position(),
                    text.selection_bounds()
                );
            }
        }
        // "nav content|sidebar": which pane a collapsed window shows.
        "nav" => win.imp().split_view.set_show_content(id == "content"),
        _ => return false,
    }
    true
}

/// The QA scripts' keyboard: each token is `mod+...+key` (`ctrl+shift+k`,
/// `Return`) or `text:words_with_underscores_for_spaces`, pressed through
/// XTest on this process's X display.
#[cfg(debug_assertions)]
fn debug_xkeys(spec: &str) {
    use crate::device_links::webrtc::remote_desktop_portal::DesktopSession;
    let Ok(x) = crate::device_links::webrtc::x11_desktop::X11Desktop::connect() else {
        debug_log!("[E2E] xkeys: no X display");
        return;
    };
    let keysym = |name: &str| -> i32 {
        match name {
            "Return" => 0xff0d,
            "Tab" => 0xff09,
            "space" => 0x20,
            "Escape" => 0xff1b,
            "Left" => 0xff51,
            "Up" => 0xff52,
            "Right" => 0xff53,
            "Down" => 0xff54,
            "ctrl" => 0xffe3,
            "shift" => 0xffe1,
            "alt" => 0xffe9,
            other => other.chars().next().map_or(0, |c| c as i32),
        }
    };
    let tap = |sym: i32| {
        let _ = x.keyboard_keysym(sym, true);
        let _ = x.keyboard_keysym(sym, false);
        std::thread::sleep(std::time::Duration::from_millis(25));
    };
    for token in spec.split_whitespace() {
        if let Some(text) = token.strip_prefix("text:") {
            for c in text.chars() {
                tap(if c == '_' { ' ' as i32 } else { c as i32 });
            }
            continue;
        }
        let parts: Vec<&str> = token.split('+').collect();
        let (key, mods) = parts.split_last().unwrap_or((&"", &[]));
        for held in mods {
            let _ = x.keyboard_keysym(keysym(held), true);
        }
        tap(keysym(key));
        for held in mods.iter().rev() {
            let _ = x.keyboard_keysym(keysym(held), false);
        }
        std::thread::sleep(std::time::Duration::from_millis(60));
    }
    debug_log!("[E2E] xkeys done: {spec}");
}

#[cfg(debug_assertions)]
fn debug_screenshot(main: &DesklinkWindow, path: &str) {
    let base = std::path::Path::new(path);
    let stem = base
        .file_stem()
        .map(|stem| stem.to_string_lossy().to_string())
        .unwrap_or_default();
    for window in gtk::Window::list_toplevels() {
        let Ok(window) = window.downcast::<gtk::Window>() else {
            continue;
        };
        if !window.is_visible() {
            continue;
        }
        let target = if window == *main.upcast_ref::<gtk::Window>() {
            base.to_path_buf()
        } else {
            let title: String = window
                .title()
                .unwrap_or_default()
                .chars()
                .map(|c| if c.is_alphanumeric() { c } else { '-' })
                .collect();
            base.with_file_name(format!("{stem}-{}.png", title.to_lowercase()))
        };
        let Some(renderer) = window.renderer() else {
            continue;
        };
        let paintable = gtk::WidgetPaintable::new(Some(&window));
        let snapshot = gtk::Snapshot::new();
        paintable.snapshot(
            &snapshot,
            f64::from(window.width()),
            f64::from(window.height()),
        );
        // Open popovers are their own surfaces: drawn in under their button.
        for popover in named_popovers(window.upcast_ref()) {
            let Some(bounds) = popover
                .parent()
                .and_then(|anchor| anchor.compute_bounds(&window))
            else {
                continue;
            };
            snapshot.save();
            snapshot.translate(&gtk::graphene::Point::new(
                bounds.x(),
                bounds.y() + bounds.height(),
            ));
            gtk::WidgetPaintable::new(Some(&popover)).snapshot(
                &snapshot,
                f64::from(popover.width()),
                f64::from(popover.height()),
            );
            snapshot.restore();
        }
        let Some(node) = snapshot.to_node() else {
            continue;
        };
        let texture = renderer.render_texture(node, None);
        match texture.save_to_png(&target) {
            Ok(()) => debug_log!("[E2E] screenshot {}", target.display()),
            Err(error) => debug_log!("[E2E] screenshot failed: {error}"),
        }
    }
}

/// The popovers open under `root`.
#[cfg(debug_assertions)]
fn named_popovers(root: &gtk::Widget) -> Vec<gtk::Popover> {
    let mut found = Vec::new();
    let mut stack = vec![root.clone()];
    while let Some(widget) = stack.pop() {
        if let Some(popover) = widget.downcast_ref::<gtk::Popover>() {
            if popover.is_visible() {
                found.push(popover.clone());
            }
        }
        let mut child = widget.first_child();
        while let Some(next) = child {
            child = next.next_sibling();
            stack.push(next);
        }
    }
    found
}

#[cfg(debug_assertions)]
pub(super) fn debug_fake_device(kind: &str, name: &str) -> DeviceView {
    use crate::device_links::device::{BatteryStatus, DesktopSession, DeviceNotification};
    use crate::device_links::device_info::DeviceInfo;
    // "KIND:TYPE", e.g. "paired:tablet"; the type defaults to a phone.
    let (kind, device_type) = kind.split_once(':').unwrap_or((kind, "phone"));
    let info = DeviceInfo {
        id: format!("fake{:028x}", glib::monotonic_time()),
        name: if name.is_empty() { "Pixel 9" } else { name }.replace('_', " "),
        device_type: device_type.to_string(),
        protocol_version: 9,
        incoming_capabilities: FeatureRegistry::desktop().outgoing_capabilities().to_vec(),
        outgoing_capabilities: FeatureRegistry::desktop().incoming_capabilities().to_vec(),
        instance_id: None,
        app_version: None,
    };
    let mut device = DeviceView::from_info(&info, "192.168.1.41".to_string(), true);
    device.battery_status = Some(BatteryStatus {
        current_charge: 82,
        is_charging: true,
        battery_quantity: None,
        threshold_event: None,
        charge_time_remaining: Some(25 * 60_000),
    });
    let now_ms = gtk::glib::real_time() / 1000;
    device.notifications = vec![
        // A long, multi-paragraph message with buttons (the owner's
        // WhatsApp screenshot).
        DeviceNotification {
            id: "4".into(),
            app_name: "WhatsApp".into(),
            title: "Writers' Circle".into(),
            text: "⏳ Just a few hours left to participate in this week's challenge!\n\n\
                   Choose from our 4 prompts:\n\
                   ♦ The Feeling of Home 🏡\n\
                   ♦ A Letter to My Younger Self ✉️\n\
                   ♦ The Last Train 🚆\n\
                   ♦ Colours of the Monsoon 🌧️\n\n\
                   Submit your entry before midnight. Winners are announced on Sunday \
                   and featured on our page.\n\nHappy writing! ✍️"
                .into(),
            ticker: String::new(),
            is_clearable: true,
            request_reply_id: Some("r4".into()),
            actions: vec!["Mark as read".into(), "Mute".into()],
            silent: false,
            time: now_ms - 2 * 60_000,
            app_icon: fake_app_icon(0x25d366ff),
        },
        DeviceNotification {
            id: "1".into(),
            app_name: "Signal".into(),
            title: "Mara".into(),
            text: "Are you deploying today?".into(),
            ticker: String::new(),
            is_clearable: true,
            request_reply_id: Some("r1".into()),
            actions: vec![],
            silent: false,
            time: now_ms - 5 * 60_000,
            app_icon: fake_app_icon(0x3a76f0ff),
        },
        DeviceNotification {
            id: "2".into(),
            app_name: "GitHub".into(),
            title: "Pull request approved".into(),
            text: "derpx06/DeskLink #48 · rate limiting".into(),
            ticker: String::new(),
            is_clearable: true,
            request_reply_id: None,
            actions: vec![],
            silent: false,
            time: now_ms - 40 * 60_000,
            app_icon: None,
        },
        DeviceNotification {
            id: "3".into(),
            app_name: "Calendar".into(),
            title: "Standup in 10 minutes".into(),
            text: "Meet · 11:00 – 11:15".into(),
            ticker: String::new(),
            is_clearable: true,
            request_reply_id: None,
            actions: vec!["Snooze".into()],
            silent: false,
            time: now_ms - 90 * 60_000,
            app_icon: None,
        },
    ];
    match kind {
        "unreachable" | "unreachable-eof" => {
            device.status = DeviceStatus::Unreachable;
            // Away for an hour, as a phone that went out of range is.
            device.last_seen = Some(crate::device_links::connection_log::now() - 3600);
            if kind == "unreachable-eof" {
                device.last_error = Some("the handshake failed: unexpected EOF".to_string());
            }
        }
        "pair-request" => {
            device.trusted = false;
            device.status = DeviceStatus::PairRequestedByPeer;
            device.verification_key = Some("482913".to_string());
        }
        "requested" => {
            device.trusted = false;
            device.status = DeviceStatus::PairRequested;
            device.verification_key = Some("482913".to_string());
        }
        "discovered" => {
            device.trusted = false;
            device.status = DeviceStatus::Discovered;
        }
        // A request that was refused: nearby again, with the reason.
        "refused" => {
            device.trusted = false;
            device.status = DeviceStatus::Discovered;
            device.last_error = Some(pairing::NOT_ACCEPTED.to_string());
        }
        "connecting" => device.status = DeviceStatus::Paired,
        _ => {
            device.status = DeviceStatus::Paired;
            device.feature_transport_ready = true;
            device.connection_path = Some(
                match kind {
                    "paired-turn" => "turn",
                    _ => "lan",
                }
                .into(),
            );
            // "paired-new": just paired, nothing allowed yet.
            device.terminal_access = kind != "paired-new";
            if kind == "paired-new" {
                device.notification_access = Some(false);
            }
            device.rtt_ms = Some(9);
            if kind == "live" || kind == "extended" {
                device.terminal_sessions_attached = 1;
                device.desktop_session = DesktopSession::Viewing;
            }
            // "extended": a second display, with sound going and a capture
            // asked for — every bar the overview can show.
            if kind == "extended" {
                device.desktop_extended = true;
                device.sound = Some(crate::device_links::device::SoundUse {
                    sending: Some("system".into()),
                    hearing: Some("mic".into()),
                    hearing_live: true,
                });
                device.capture = Some(crate::device_links::device::CaptureUse {
                    kind: "scan".into(),
                    receiving: false,
                });
            }
        }
    }
    // "ringing" / "talking": a call on the phone.
    use crate::device_links::device::{CallState, CallStatus, PhoneCall};
    let call = match kind {
        "ringing" => Some(CallState::Ringing),
        "talking" => Some(CallState::Talking),
        _ => None,
    };
    if let Some(state) = call {
        device.call_status = Some(CallStatus {
            calls: vec![PhoneCall {
                call_id: "fake-call".into(),
                state,
                contact_name: Some("Mara Lindqvist".into()),
                number: Some("+46 70 123 45 67".into()),
                started_at: (state == CallState::Talking)
                    .then(|| glib::real_time() / 1000 - 83_000),
                incoming: true,
            }],
            muted: false,
            silenced: false,
        });
    }
    device
}

/// A made-up app icon (a disc of `rgba` with a white dot), kept the way the
/// phone's are; its hash.
#[cfg(debug_assertions)]
fn fake_app_icon(rgba: u32) -> Option<String> {
    use base64::Engine;
    let mut pixels = vec![0u8; 64 * 64 * 4];
    for (index, pixel) in pixels.chunks_mut(4).enumerate() {
        let (x, y) = ((index % 64) as f64 - 31.5, (index / 64) as f64 - 31.5);
        let distance = (x * x + y * y).sqrt();
        if distance < 13.0 {
            pixel.copy_from_slice(&[255, 255, 255, 255]);
        } else if distance < 31.5 {
            pixel.copy_from_slice(&rgba.to_be_bytes());
        }
    }
    let pixbuf = gtk::gdk_pixbuf::Pixbuf::from_bytes(
        &gtk::glib::Bytes::from_owned(pixels),
        gtk::gdk_pixbuf::Colorspace::Rgb,
        true,
        8,
        64,
        64,
        64 * 4,
    );
    let png = pixbuf.save_to_bufferv("png", &[]).ok()?;
    let hash = crate::device_links::app_icons::hash_of(&png);
    crate::device_links::app_icons::store(
        &hash,
        &base64::engine::general_purpose::STANDARD.encode(&png),
    )
    .then_some(hash)
}
