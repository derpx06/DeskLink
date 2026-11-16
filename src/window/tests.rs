//! Tests of the window: page rules, device listing, toasts, errors, saved-in
//! words, layout and the debug verbs, partly on a real GTK display (Xvfb).

#[test]
fn a_remembered_window_size_is_read_only_when_sane() {
    assert_eq!(
        super::parse_window_size("1180 760 0"),
        Some((1180, 760, false))
    );
    assert_eq!(
        super::parse_window_size("900 600 1\n"),
        Some((900, 600, true))
    );
    assert_eq!(super::parse_window_size("10 10 0"), None);
    assert_eq!(super::parse_window_size("wide"), None);
    assert_eq!(super::parse_window_size(""), None);
}

#[test]
fn pairing_errors_are_said_in_plain_words_and_the_reinstall_hint_is_right() {
    for (raw, needle) in [
        (
            "Trusted device certificate changed",
            "pair it again by scanning the QR code",
        ),
        ("The QR pairing code expired", "Scan the new one"),
        ("No QR pairing code is active", "Open the pairing QR code"),
        ("Invalid QR pairing proof", "not from this computer"),
    ] {
        let (words, _) = super::friendly_error(raw);
        assert!(words.contains(needle), "{raw}: {words}");
        assert!(
            !words.to_lowercase().contains("unpair"),
            "never tells the user to unpair first: {words}"
        );
    }
    assert!(
        super::friendly_error("Trusted device certificate changed").1,
        "shown as a banner"
    );
}

use super::*;

/// A press anywhere holds every redraw until it is released, so the
/// button under the pointer is still the one released on (the owner's
/// "clicks don't work"). (Skipped without a display.)
#[test]
fn nothing_is_redrawn_between_press_and_release() {
    crate::test_display::run(|| {
        let resources = crate::load_resources().expect("the bundled resources");
        gio::resources_register(&resources);
        let app = adw::Application::builder()
            .application_id("derx06.desklink.com.test")
            .build();
        let window = DesklinkWindow::new(&app);
        let (daemon, _commands) = DaemonHandle::detached();
        daemon.set_load_state_for_tests(crate::device_links::daemon::handle::LOAD_KNOWN);
        window.set_daemon(daemon);
        let phone = debug_fake_device("paired", "Pixel 9");
        let id = phone.id.clone();
        window.imp().fake_devices.borrow_mut().push(phone);
        window.select_device(&id);
        window.present();
        let pump = || {
            let context = glib::MainContext::default();
            for _ in 0..30 {
                while context.iteration(false) {}
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        };
        pump();
        window.refresh_devices();
        pump();
        let drawn = || window.imp().snapshots.borrow().contains_key("home");
        assert!(drawn(), "Home is drawn");
        window.imp().snapshots.borrow_mut().clear();
        window.imp().pointer_held.set(true);
        window.refresh_devices();
        assert!(!drawn(), "held: nothing is redrawn");
        window.imp().pointer_held.set(false);
        window.refresh_devices();
        assert!(drawn(), "released: redrawn");
    });
}

/// The "This phone's identity changed" notice is a banner that stays
/// until the situation does, with one button, Details, which opens the
/// raw error to read or copy. (Skipped without a display.)
#[test]
fn the_identity_notice_is_a_banner_whose_details_button_shows_the_error() {
    crate::test_display::run(|| {
        let resources = crate::load_resources().expect("the bundled resources");
        gio::resources_register(&resources);
        let app = adw::Application::builder()
            .application_id("derx06.desklink.com.test")
            .build();
        let window = DesklinkWindow::new(&app);
        let (daemon, _commands) = DaemonHandle::detached();
        window.set_daemon(daemon);
        let raw = "Trusted device certificate changed";
        window.report_error(raw, false);
        let banner = &window.imp().error_banner;
        assert!(banner.is_revealed(), "a lasting state is a banner");
        assert!(banner.title().contains("identity changed"));
        assert_eq!(banner.button_label().as_deref(), Some("Details"));
        banner.emit_by_name::<()>("button-clicked", &[]);
        let dialog = window
            .visible_dialog()
            .and_downcast::<adw::AlertDialog>()
            .expect("Details opens the error");
        assert_eq!(dialog.heading().as_deref(), Some("Error details"));
        assert!(dialog.body().contains("identity changed"));
        dialog.close();
    });
}

/// A standing problem stays in the banner, comes back at every poll after
/// an error has had the banner, and goes only when dismissed or fixed.
#[test]
fn a_standing_problem_stays_in_the_banner_until_it_is_dismissed_or_fixed() {
    use crate::platform::{evaluate, Probe};
    let _held = crate::platform::TEST_REPORT
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    crate::test_display::run(|| {
        let resources = crate::load_resources().expect("the bundled resources");
        gio::resources_register(&resources);
        let app = adw::Application::builder()
            .application_id("derx06.desklink.com.test")
            .build();
        let window = DesklinkWindow::new(&app);
        let banner = window.imp().error_banner.clone();
        let broken = |writable: bool, plugins: bool| {
            let mut probe = Probe {
                config_writable: writable,
                ..Probe::default()
            };
            if plugins {
                probe.elements = ["webrtcbin", "dtlsdec", "dtlssrtpenc", "nicesrc"]
                    .map(String::from)
                    .into();
            }
            crate::platform::set_report_for_tests(evaluate(&probe));
        };
        // A read-only home: shown, and still shown at the next poll.
        broken(false, true);
        assert!(window.assert_sticky_notice());
        assert!(banner.is_revealed() && banner.title().contains("configuration folder"));
        assert_eq!(banner.button_label().as_deref(), Some("Dismiss"));
        assert!(window.assert_sticky_notice(), "asserted again");
        // An error takes the banner; the problem waits, then returns.
        window.report_error("Trusted device certificate changed", false);
        assert!(banner.title().contains("identity changed"));
        assert!(!window.assert_sticky_notice(), "does not replace an error");
        assert!(banner.title().contains("identity changed"));
        banner.set_revealed(false);
        assert!(window.assert_sticky_notice(), "back once the error is gone");
        // Dismissed by the person: gone for this run.
        banner.emit_by_name::<()>("button-clicked", &[]);
        assert!(!banner.is_revealed());
        assert!(!window.assert_sticky_notice());
        assert!(!banner.is_revealed());
        // Another problem still shows, and a fix hides its banner.
        broken(true, false);
        assert!(window.assert_sticky_notice());
        assert!(banner.title().contains("Phones cannot connect"));
        // What to install is behind the button, never in the words.
        assert!(!banner.title().contains("Install:") && !banner.title().contains("sudo"));
        assert!(banner.title().contains(INSTALL_HINT));
        assert_eq!(banner.button_label().as_deref(), Some("Copy command"));
        broken(true, true);
        assert!(!window.assert_sticky_notice());
        assert!(!banner.is_revealed());
        crate::platform::clear_report_for_tests();
    });
}

/// A label that is cut short gives its whole text on hover; one that
/// wraps is never cut. (Needs a display.)
#[test]
fn cut_labels_give_their_whole_text_and_wrapped_ones_are_never_cut() {
    crate::test_display::run(|| {
        let cut = ellipsized("A long name that may not fit", &[]);
        assert_eq!(cut.ellipsize(), gtk::pango::EllipsizeMode::End);
        assert!(cut.has_tooltip(), "the whole text is there to hover");
        let words = wrapped("A reason the person must read in full", &[]);
        assert!(words.wraps());
        assert_eq!(words.ellipsize(), gtk::pango::EllipsizeMode::None);
    });
}

use crate::device_links::device_info::DeviceInfo;
use crate::device_links::packet::{PACKET_TYPE_MOUSEPAD_REQUEST, PACKET_TYPE_PING};

fn phone() -> DeviceView {
    let info = DeviceInfo {
        id: "0123456789abcdef0123456789abcdef".to_string(),
        name: "Phone".to_string(),
        device_type: "phone".to_string(),
        protocol_version: 9,
        incoming_capabilities: vec![PACKET_TYPE_PING.to_string()],
        outgoing_capabilities: Vec::new(),
        instance_id: None,
        app_version: None,
    };
    DeviceView::from_info(&info, "127.0.0.1".to_string(), true)
}

#[test]
fn a_fallback_choice_gives_way_to_a_reachable_phone_but_a_chosen_one_stays() {
    let with = |id: &str, reachable: bool| {
        let mut device = phone();
        device.id = id.to_string();
        device.status = if reachable {
            DeviceStatus::Paired
        } else {
            DeviceStatus::Unreachable
        };
        device
    };
    let s = |id: &str| Some(id.to_string());
    // F1: the remembered id is stale; the emulator is up first, unreachable.
    let start = [with("emu", false), with("realme", false)];
    assert_eq!(device_to_show(Some("stale"), None, &start), s("emu"));
    let up = [with("emu", false), with("realme", true)];
    assert_eq!(device_to_show(Some("stale"), Some("emu"), &up), s("realme"));
    // An empty first list does not lose the remembered phone.
    assert_eq!(device_to_show(Some("realme"), None, &[]), None);
    assert_eq!(device_to_show(Some("realme"), None, &start), s("realme"));
    // The person's choice is never overridden, reachable or not.
    assert_eq!(device_to_show(Some("emu"), Some("emu"), &up), s("emu"));
    // A reachable fallback is kept; nothing reachable keeps the current one.
    let both = [with("a", true), with("b", true)];
    assert_eq!(device_to_show(None, Some("b"), &both), s("b"));
    let none = [with("a", false), with("b", false)];
    assert_eq!(device_to_show(None, Some("b"), &none), s("b"));
    // The chosen phone was unpaired: the fallback again.
    assert_eq!(device_to_show(Some("gone"), Some("gone"), &up), s("realme"));
}

#[test]
fn feature_tiles_require_both_a_local_sender_and_peer_receiver() {
    let device = phone();
    assert!(can_send_feature(&device, PACKET_TYPE_PING));
    assert!(!can_send_feature(&device, PACKET_TYPE_MOUSEPAD_REQUEST));
}

/// The page used to render a hardcoded green "● Connected" for every
/// paired device, so an unreachable phone read as connected right next to
/// a sidebar row that said otherwise. Both now derive from `Presentation`.
#[test]
fn status_presentation_never_reports_an_unreachable_device_as_connected() {
    use crate::link_glyph::Presentation;
    let mut device = phone();
    device.status = DeviceStatus::Unreachable;
    assert_eq!(Presentation::of(&device).word, "Can't reach");

    // Paired but the WebRTC handover has not finished.
    device.status = DeviceStatus::Paired;
    device.feature_transport_ready = false;
    assert_eq!(Presentation::of(&device).word, "Connecting…");

    device.feature_transport_ready = true;
    assert_eq!(Presentation::of(&device).word, "Connected");
}

/// Every action on an offline or connecting phone says why it is
/// unavailable, rather than failing after the click.
#[test]
fn actions_say_why_they_are_unavailable() {
    let mut device = phone();
    device.status = DeviceStatus::Unreachable;
    assert_eq!(
        unavailable_reason(&device).as_deref(),
        Some("Phone is offline")
    );
    device.status = DeviceStatus::Paired;
    assert_eq!(
        unavailable_reason(&device).as_deref(),
        Some("Connecting to Phone…")
    );
    device.feature_transport_ready = true;
    assert_eq!(unavailable_reason(&device), None);
}

#[test]
fn the_live_banner_says_what_the_phone_is_doing() {
    use crate::device_links::device::DesktopSession;
    let mut device = phone();
    assert_eq!(session_summary(&device), None);
    device.terminal_sessions_attached = 1;
    device.desktop_session = DesktopSession::Viewing;
    assert_eq!(
        session_summary(&device).as_deref(),
        Some("in a terminal, viewing your screen")
    );
    device.terminal_sessions_attached = 2;
    device.desktop_session = DesktopSession::Controlling;
    assert_eq!(
        session_summary(&device).as_deref(),
        Some("in 2 terminals, controlling your screen")
    );
}

#[test]
fn the_pairing_code_reads_as_two_groups_of_three() {
    assert_eq!(spaced_code("482913"), "482 913");
    assert_eq!(spaced_code("012345"), "012 345");
    // Anything else is shown as it came.
    assert_eq!(spaced_code("12345"), "12345");
}

#[test]
fn daemon_errors_read_as_plain_words() {
    let (offline, lasting) = friendly_error("Network is unreachable (os error 101)");
    assert!(is_offline_message(&offline), "{offline}");
    assert!(lasting, "being offline is a state, shown as a banner");

    let (refused, lasting) =
        friendly_error("connect 10.0.0.2:1716: Connection refused (os error 111)");
    assert!(refused.starts_with("The device refused"), "{refused}");
    assert!(!lasting);
    assert!(friendly_error("Refusing downgraded protocol from Pixel: 8")
        .0
        .contains("Update DeskLink"));
    assert!(friendly_error("operation timed out")
        .0
        .contains("timed out"));
    assert!(friendly_error("Permission denied (os error 13)")
        .0
        .contains("not allowed"));
    // Error 1 is not error 101 or 13.
    assert!(friendly_error("Operation not permitted (os error 1)")
        .0
        .contains("not allowed"));

    // The pairing refusals are sentences, not the daemon's wording.
    for raw in [
        crate::device_links::pairing::CLOCKS_DIFFER,
        "Expired DeskLink WebRTC signaling message: the two devices' clocks differ by more than 5 minutes; enable automatic time on both",
        "Too many wrong QR pairing proofs from this device",
        "Malformed QR pairing proof",
        "Too many links are waiting to pair by QR code",
    ] {
        let (words, _) = friendly_error(raw);
        assert_ne!(words, raw, "{raw}");
        assert!(words.ends_with('.'), "{words}");
    }

    // Anything else keeps its words but loses the errno tail.
    assert_eq!(
        friendly_error("Pairing failed (os error 71)"),
        ("Pairing failed".to_string(), false)
    );
    assert_eq!(friendly_error("Pairing failed").0, "Pairing failed");
}

/// Words that merely appear in an unrelated error are not rewritten into
/// advice about the network, a certificate or the settings.
#[test]
fn unrelated_errors_keep_their_own_words() {
    for raw in [
        "EOF while parsing a value at line 1 column 0",
        "Could not read the file: end of file reached in the notes",
        "Cannot resolve the symlink for that folder",
        "Dissolved 3 sessions",
        "No space left on the screen for another tile",
        "Verify failed: the checksum of the copy differs",
    ] {
        assert_eq!(friendly_error(raw), (raw.to_string(), false), "{raw}");
    }
    // The disk errors say "settings" only for a failed settings save.
    let full = "No space left on device (os error 28)";
    assert!(!friendly_error(full).0.contains("settings"));
    let saved = format!("{}{full}", crate::device_links::config::SAVE_FAILED);
    assert!(friendly_error(&saved).0.contains("settings"));
    let read_only = format!(
        "{}Read-only file system (os error 30)",
        crate::device_links::config::SAVE_FAILED
    );
    assert!(friendly_error(&read_only).0.contains("read-only"));
    // The real ones still map.
    assert!(
        friendly_error("error:0A000086:SSL routines::certificate verify failed")
            .0
            .contains("identity could not be checked")
    );
    assert!(friendly_error("failed to fill whole buffer")
        .0
        .contains("dropped"));
    assert!(friendly_error("Temporary failure in name resolution")
        .0
        .contains("could not find"));
}

#[test]
fn every_device_type_has_its_own_glyph_and_name() {
    let kinds: Vec<_> = ["phone", "tablet", "tv", "laptop", "desktop", "unknown"]
        .iter()
        .map(|kind| device_kind(kind))
        .collect();
    let glyphs: std::collections::HashSet<_> = kinds.iter().map(|(glyph, _)| glyph).collect();
    assert_eq!(glyphs.len(), 6, "a tablet must not look like a phone");
    assert_eq!(device_kind("tablet").1, "Tablet");
    assert_eq!(device_kind("TV").1, "TV");
    assert_eq!(device_kind("toaster"), device_kind("unknown"));
    // Every glyph is bundled in the resources.
    let resources = include_str!("../desklink.gresource.xml");
    for (glyph, _) in kinds {
        assert!(
            resources.contains(&format!("{glyph}.svg")),
            "{glyph} is not bundled"
        );
    }
}

/// The sidebar, top to bottom: Home, Files, Notes & clipboard,
/// Notifications, Terminals, Contacts, then the Settings footer.
#[test]
fn the_sidebar_lists_the_six_pages_then_settings() {
    let titles: Vec<_> = PAGES.iter().map(|(_, title, _)| *title).collect();
    assert_eq!(
        titles,
        [
            "Home",
            "Files",
            "Notes & clipboard",
            "Notifications",
            "Terminals",
            "Contacts",
            "Settings"
        ]
    );
    let names: Vec<_> = PAGES.iter().map(|(name, _, _)| *name).collect();
    assert_eq!(&names[MAIN_PAGES..], ["settings"]);
    assert_eq!(MAIN_PAGES, 6);
}

/// Ctrl+1 to Ctrl+6 open the six pages in sidebar order, Settings keeps
/// Ctrl+, and the dialog that lists them says the same keys.
#[test]
fn the_page_shortcuts_follow_the_sidebar_and_the_dialog_lists_them() {
    let names: Vec<_> = PAGES
        .iter()
        .take(MAIN_PAGES)
        .map(|(name, _, _)| *name)
        .collect();
    let bound: Vec<_> = PAGE_ACCELS.iter().map(|(name, _)| *name).collect();
    assert_eq!(names, bound);
    let mut seen = std::collections::HashSet::new();
    let dialog = include_str!("../shortcuts-dialog.ui");
    for (index, (_, accel)) in PAGE_ACCELS.iter().enumerate() {
        assert_eq!(*accel, format!("<control>{}", index + 1));
        assert!(seen.insert(*accel), "{accel} twice");
        assert!(
            dialog.contains(&format!("&lt;Control&gt;{}<", index + 1)),
            "the dialog lists Ctrl+{}",
            index + 1
        );
    }
    for words in [
        "Send files",
        "&lt;Control&gt;o",
        "&lt;Control&gt;comma",
        "Pair another phone",
    ] {
        assert!(dialog.contains(words), "{words}");
    }
    // Sentence case in the dialog, as everywhere.
    assert!(!dialog.contains("Pair New Device") && !dialog.contains("Close Window"));
}

/// What the shortcuts dialog lists is what the application binds: every
/// action it names has a key in `application::ACCELS`, every accelerator
/// it types out is one the window binds, no key is bound twice, and
/// Ctrl+K (Find anything) is listed and named on every search button.
#[test]
fn the_shortcuts_dialog_lists_the_keys_the_application_binds() {
    use crate::application::ACCELS;
    let dialog = include_str!("../shortcuts-dialog.ui");
    let value = |line: &str, property: &str| -> Option<String> {
        let open = format!("<property name=\"{property}\"");
        let at = line.find(&open)?;
        let rest = &line[at..];
        let text = &rest[rest.find('>')? + 1..rest.find("</property>")?];
        Some(
            text.replace("&lt;", "<")
                .replace("&gt;", ">")
                .replace("&amp;", "&"),
        )
    };
    let mut actions = Vec::new();
    let mut typed = Vec::new();
    for line in dialog.lines() {
        actions.extend(value(line, "action-name"));
        typed.extend(value(line, "accelerator"));
    }
    assert!(
        actions.len() >= 6 && typed.len() >= 7,
        "{actions:?} {typed:?}"
    );
    for action in &actions {
        assert!(
            ACCELS.iter().any(|(bound, _)| bound == action),
            "{action} is listed but has no key"
        );
    }
    for accel in &typed {
        assert!(
            ACCELS
                .iter()
                .map(|(_, accel)| *accel)
                .chain(PAGE_ACCELS.iter().map(|(_, accel)| *accel))
                .chain(terminals::TERMINAL_ACCELS.iter().map(|(_, accel)| *accel))
                .any(|bound| bound.eq_ignore_ascii_case(accel)),
            "{accel} is typed in the dialog but bound to nothing"
        );
    }
    // Every key of the page's terminal is listed, by its own name.
    for (name, accel) in terminals::TERMINAL_ACCELS {
        assert!(
            typed
                .iter()
                .any(|listed| listed.eq_ignore_ascii_case(accel)),
            "{accel} ({name}) is not in the dialog"
        );
        assert!(dialog.contains(&format!(">{name}<")), "{name}");
    }
    let mut seen = std::collections::HashSet::new();
    for accel in ACCELS
        .iter()
        .map(|(_, accel)| *accel)
        .chain(PAGE_ACCELS.iter().map(|(_, accel)| *accel))
    {
        assert!(seen.insert(accel), "{accel} is bound twice");
    }
    // Find anything: bound, listed by its action, named on its buttons.
    assert!(ACCELS.contains(&("win.palette", PALETTE_ACCEL)));
    assert!(actions.iter().any(|action| action == "win.palette"));
    assert_eq!(PALETTE_ACCEL, "<control>k");
    // Four ways in: the welcome header, the sidebar's header and its search
    // field above Settings, and the page header once the sidebar is folded
    // away. Each says Ctrl+K.
    let ui = include_str!("../window.ui");
    let buttons: Vec<&str> = ui
        .split("<object class=\"GtkButton\"")
        .filter(|button| {
            button
                .split("<object ")
                .next()
                .is_some_and(|own| own.contains("win.palette"))
        })
        .collect();
    assert_eq!(buttons.len(), 4, "{buttons:?}");
    for button in &buttons {
        assert!(button.contains("Find anything (Ctrl+K)"), "{button}");
    }
    // The field sits at the bottom of the sidebar, just above Settings.
    let at = |id: &str| ui.find(&format!("id=\"{id}\"")).unwrap_or_default();
    assert!(at("nav_list") < at("sidebar_search") && at("sidebar_search") < at("footer_list"));
    // The field shows the key it is opened by, and is read as the same name.
    let field = ui
        .split("id=\"sidebar_search\"")
        .nth(1)
        .expect("the sidebar has a search field");
    let field = field.split("footer_list").next().unwrap_or_default();
    assert!(field.contains(
        "<property name=\"label\" translatable=\"yes\">Find anything (Ctrl+K)</property>"
    ));
    assert!(field.contains(&format!(
        "<property name=\"accelerator\">{}</property>",
        PALETTE_ACCEL.replace('<', "&lt;").replace('>', "&gt;")
    )));
    // The plain list of older libadwaita says it too.
    assert!(include_str!("../application/actions.rs").contains("(\"Find anything\", \"Ctrl+K\")"));
}

#[test]
fn page_shortcuts_wait_for_a_paired_phone_and_for_dialogs() {
    assert!(page_shortcut_applies("files", false, true));
    assert!(!page_shortcut_applies("files", true, true), "over a dialog");
    assert!(
        !page_shortcut_applies("files", false, false),
        "still pairing"
    );
    assert!(!page_shortcut_applies("home", false, false));
    assert!(
        page_shortcut_applies("notes", false, false),
        "the user's own"
    );
    assert!(!page_shortcut_applies("notes", true, false));
}

#[test]
fn the_drop_words_name_the_phone() {
    assert_eq!(sidebar::drop_words("Pixel 9"), "Drop to send to Pixel 9");
    assert_eq!(sidebar::drop_words(""), "Drop to send");
}

/// `win.send-again` acts only on a retry that was offered.
#[test]
fn send_again_ignores_a_path_nobody_offered() {
    assert!(!take_retry("phone\n/etc/passwd"));
    offer_retry("phone\n/home/u/a.bin");
    assert!(
        !take_retry("phone\n/etc/passwd"),
        "another path stays refused"
    );
    assert!(take_retry("phone\n/home/u/a.bin"), "the offered one works");
    assert!(!take_retry("phone\n/home/u/a.bin"), "once");
    for n in 0..MAX_RETRIES + 5 {
        offer_retry(&format!("p\n/f{n}"));
    }
    assert!(!take_retry("p\n/f0"), "the oldest offers are forgotten");
    assert!(take_retry(&format!("p\n/f{}", MAX_RETRIES + 4)));
}

#[test]
fn a_saved_file_says_downloads_even_when_downloads_is_a_link() {
    use std::path::Path;
    let base = std::env::temp_dir().join(format!("desklink-saved-{}", uuid::Uuid::new_v4()));
    let real = base.join("disk/files");
    std::fs::create_dir_all(&real).unwrap();
    let link = base.join("Downloads");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let file = std::fs::canonicalize(&real).unwrap().join("a.pdf");
    assert_eq!(saved_in_with(&file, Some(&link)), "Downloads");
    assert_eq!(saved_in_with(&file, None), "files");
    assert_eq!(
        saved_in_with(Path::new("/home/u/Music/a.pdf"), Some(&link)),
        "Music"
    );
    let _ = std::fs::remove_dir_all(&base);
}

/// Every error on the Home hero reads as a sentence; the library's words
/// are only in the details.
#[test]
fn hero_errors_are_plain_words_with_the_details_kept_aside() {
    let cases = [
        (
            "the handshake failed: unexpected EOF",
            "dropped while it was being set up",
        ),
        (
            "TLS error: peer closed connection without sending close_notify (eof)",
            "dropped while it was being set up",
        ),
        ("early eof", "dropped while it was being set up"),
        ("Connection timed out (os error 110)", "timed out"),
        (
            "Connection refused (os error 111)",
            "refused the connection",
        ),
        ("Connection reset by peer (os error 104)", "interrupted"),
        ("Trusted device certificate changed", "identity changed"),
        (
            "invalid peer certificate: UnknownIssuer",
            "identity could not be checked",
        ),
        (
            "failed to lookup address information: Name or service not known",
            "could not find",
        ),
        ("Network is unreachable (os error 101)", "offline"),
    ];
    for (raw, expected) in cases {
        let (words, details) = hero_error_words(raw);
        assert!(
            words.to_lowercase().contains(expected),
            "{raw:?} read as {words:?}"
        );
        assert!(
            !words.contains("EOF") && !words.contains("os error"),
            "{words}"
        );
        assert!(details.is_some(), "{raw:?} keeps its details");
    }
    // Text nobody wrote for a person is never shown as is.
    let (words, details) = hero_error_words("serde_json::Error: expected value at src/a.rs:4");
    assert!(words.starts_with("Something went wrong"), "{words}");
    assert!(details.is_some_and(|raw| raw.contains("serde_json")));
    // A sentence already written for people stays, with no details.
    let (words, details) = hero_error_words("Update DeskLink on Pixel 9");
    assert_eq!(
        (words.as_str(), details),
        ("Update DeskLink on Pixel 9", None)
    );
}

/// A worker that panicked ends the wait; a slow one is waited for.
#[test]
fn a_dead_worker_ends_the_wait_for_its_result() {
    let (sender, receiver) = std::sync::mpsc::channel::<u32>();
    assert_eq!(try_result(&receiver), Poll::Waiting);
    sender.send(7).unwrap();
    assert_eq!(try_result(&receiver), Poll::Done(7));
    let worker = std::thread::spawn(move || {
        let _sender = sender;
        panic!("the worker fell over");
    });
    assert!(worker.join().is_err());
    assert_eq!(try_result(&receiver), Poll::Gone);
}

#[test]
fn the_notes_and_clipboard_tabs_are_addressed_by_name() {
    assert_eq!(resolve_page("clipboard"), ("notes", Some("clipboard")));
    assert_eq!(resolve_page("snippets"), ("notes", Some("snippets")));
    for (name, _, _) in PAGES {
        assert_eq!(resolve_page(name), (*name, None));
    }
    let tabs: Vec<_> = notes_clipboard::TABS.iter().map(|(_, t, _)| *t).collect();
    assert_eq!(tabs, ["Notes", "Snippets", "Clipboard history"]);
    assert_eq!(notes_clipboard::tab_of("clipboard"), Some("clipboard"));
    assert_eq!(notes_clipboard::tab_of("files"), None);
}

/// Toasts never pile up: repeats are one, and only the newest few stay.
#[test]
fn toasts_are_capped_and_repeats_coalesce() {
    let live = |titles: &[&str]| titles.iter().map(|t| t.to_string()).collect::<Vec<_>>();
    assert_eq!(toast_plan(&live(&[]), "Ringing Pixel"), ToastPlan::Add);
    assert_eq!(
        toast_plan(&live(&["Ringing Pixel"]), "Ringing Pixel"),
        ToastPlan::Skip
    );
    assert_eq!(toast_plan(&live(&["a", "b"]), "c"), ToastPlan::Add);
    assert_eq!(
        toast_plan(&live(&["a", "b", "c"]), "d"),
        ToastPlan::Dismiss(1)
    );
    assert_eq!(
        toast_plan(&live(&["a", "b", "c", "d"]), "e"),
        ToastPlan::Dismiss(2)
    );
}
