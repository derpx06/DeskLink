//! Tests of the first run: when it shows, navigation, the setup rows for each
//! system check, plain words, and the pages on a real GTK display (Xvfb).

use super::*;
use crate::platform::{evaluate, Probe};

#[test]
fn the_tour_is_for_an_unpaired_computer_that_has_not_seen_it() {
    assert!(show_on_start(false, false));
    assert!(!show_on_start(true, false), "seen once");
    assert!(!show_on_start(false, true), "a paired computer never");
    assert!(!show_on_start(true, true));
}

/// The glance, three tour pages, the setup step, then (unpaired) the
/// pairing page: Next walks them in order, Back goes one back and stops
/// at the first, Skip jumps from the glance or any tour page to setup and
/// leaves from setup.
#[test]
fn the_steps_go_in_order_and_skip_lands_on_setup() {
    assert_eq!(TOUR.len(), 3);
    assert_eq!(SETUP, 4);
    assert_eq!(segment_count(true), 6, "glance, tour, setup, pairing");
    assert_eq!(segment_count(false), 5, "a replay ends at setup");
    let mut step = 0;
    let mut seen = vec![step];
    while let Some(next) = go(step, Nav::Next) {
        step = next;
        seen.push(step);
    }
    assert_eq!(seen, vec![0, 1, 2, 3, 4], "then it leaves for pairing");
    assert_eq!(go(0, Nav::Back), Some(0));
    assert_eq!(go(SETUP, Nav::Back), Some(3));
    for page in 0..SETUP {
        assert_eq!(go(page, Nav::Skip), Some(SETUP));
    }
    assert_eq!(go(SETUP, Nav::Skip), None);
    assert_eq!(
        next_label(0, true),
        "Get started",
        "the glance's one button"
    );
    assert_eq!(next_label(1, true), "Next");
    assert_eq!(next_label(SETUP, true), "Continue");
    assert_eq!(next_label(SETUP, false), "Done");
}

#[test]
fn enter_and_arrows_move_and_escape_skips() {
    use gdk::Key;
    assert_eq!(key_nav(Key::Return, false), Some(Nav::Next));
    assert_eq!(key_nav(Key::KP_Enter, false), Some(Nav::Next));
    assert_eq!(key_nav(Key::Right, false), Some(Nav::Next));
    assert_eq!(key_nav(Key::Left, false), Some(Nav::Back));
    assert_eq!(key_nav(Key::Escape, false), Some(Nav::Skip));
    assert_eq!(key_nav(Key::Left, true), Some(Nav::Next), "mirrored");
    assert_eq!(key_nav(Key::Right, true), Some(Nav::Back));
    assert_eq!(key_nav(Key::Tab, false), None, "Tab moves the focus");
    assert_eq!(key_nav(Key::space, false), None);
}

/// The glance and the tour say, word for word, what the phone's first
/// screen and tour say (`OnboardingCopyTest` pins the same lines there).
#[test]
fn the_glance_and_tour_match_the_phone() {
    assert_eq!(GLANCE_KICKER, "Welcome to DeskLink");
    assert_eq!(GLANCE_TITLE, "Your phone and computer, together");
    assert_eq!(
        GLANCE_LINES.map(|(_, line)| line),
        [
            "See and control one screen from the other",
            "Send files, and copy on one, paste on the other",
            "Phone notifications and terminals on both",
        ]
    );
    assert_eq!(GLANCE_STEPS_TITLE, "Start in 3 steps");
    assert_eq!(
        GLANCE_STEPS,
        [
            "Install DeskLink on both",
            "Pair once with a code",
            "Done: they connect on their own",
        ]
    );
    assert_eq!(
        GLANCE_FOOTER,
        "No account. Only your paired devices can connect."
    );
    assert_eq!(GLANCE_START, "Get started");
    let tour: Vec<[&str; 3]> = TOUR
        .iter()
        .map(|page| [page.kicker, page.title, page.body])
        .collect();
    assert_eq!(
        tour,
        [
            [
                "Screens",
                "See it. Steer it.",
                "Watch one screen on the other, and use your phone as a touchpad and keyboard."
            ],
            [
                "Share",
                "Files and copies, both ways",
                "Send a file in a tap, and copy on one device to paste on the other."
            ],
            [
                "Terminals",
                "Terminals that follow you",
                "Start a terminal on one device and pick it up on the other."
            ],
        ]
    );
    // One sentence a page.
    for page in &TOUR {
        assert_eq!(page.body.matches(". ").count(), 0, "{}", page.body);
    }
}

#[test]
fn the_words_are_plain() {
    let mut all: Vec<&str> = TOUR
        .iter()
        .flat_map(|page| [page.kicker, page.title, page.body])
        .collect();
    all.extend([SETUP_KICKER, SETUP_TITLE, SETUP_BODY]);
    all.extend([
        GLANCE_KICKER,
        GLANCE_TITLE,
        GLANCE_STEPS_TITLE,
        GLANCE_FOOTER,
        GLANCE_START,
    ]);
    all.extend(GLANCE_LINES.map(|(_, line)| line));
    all.extend(GLANCE_STEPS);
    for words in all {
        assert_plain(words);
    }
    for title in TOUR.iter().map(|page| page.title).chain([GLANCE_TITLE]) {
        let rest: String = title.chars().skip(1).collect();
        assert!(
            !rest.split_whitespace().any(|word| word
                .chars()
                .next()
                .is_some_and(char::is_uppercase)
                && word != "Steer"),
            "sentence case: {title}"
        );
    }
}

fn gnome_wayland() -> Probe {
    Probe {
        desktop: "GNOME".into(),
        session_type: Some("wayland".into()),
        wayland_display: true,
        x11_display: true,
        runtime_dir: true,
        session_bus: true,
        system_bus: true,
        session_names: [
            "org.freedesktop.portal.Desktop",
            "org.freedesktop.Notifications",
        ]
        .map(String::from)
        .into(),
        system_names: ["org.freedesktop.login1".to_string()].into(),
        portals: [
            "ScreenCast",
            "RemoteDesktop",
            "InputCapture",
            "Notification",
        ]
        .map(String::from)
        .into(),
        input_capture: Some(3),
        elements: [
            "webrtcbin",
            "dtlsdec",
            "dtlssrtpenc",
            "nicesrc",
            "opusenc",
            "opusdec",
            "vp8dec",
            "vp8enc",
            "pipewiresrc",
            "pipewiresink",
        ]
        .map(String::from)
        .into(),
        pipewire_socket: true,
        config_writable: true,
        ..Probe::default()
    }
}

fn ids(rows: &[SetupRow]) -> Vec<&'static str> {
    rows.iter().map(|row| row.id).collect()
}

/// A complete GNOME computer: every row in order, all ready, and what the
/// screen-sharing and Glide dialogs will ask said in GNOME's words.
#[test]
fn a_complete_gnome_computer_reads_ready_in_order() {
    let checks = evaluate(&gnome_wayland());
    let rows = setup_rows(&SetupFacts {
        checks: &checks,
        start_at_login: true,
        extension: Some(ExtensionStatus::Active),
        gnome: true,
        distro: Some(Distro::Arch),
    });
    assert_eq!(
        ids(&rows),
        [
            "autostart",
            "notifications",
            "extension",
            "share",
            "glide",
            "keys",
            "plugins"
        ]
    );
    for row in &rows {
        assert_eq!(row.status, Status::Ok, "{}: {}", row.id, row.detail);
        // What a ready computer reads: no technical words.
        assert_plain(row.title);
        assert_plain(&row.detail);
    }
    let share = &rows[3];
    assert!(
        share.detail.contains("Allow Remote Interaction"),
        "{}",
        share.detail
    );
    assert!(share.detail.contains("until DeskLink restarts"));
    assert!(rows[4].detail.contains("Capture Input"));
    assert!(rows[4]
        .detail
        .starts_with("Move your mouse across to the phone"));
    assert!(rows[5].detail.contains("No keyring"));
    assert_eq!(rows[0].fix, Fix::Switch(true));
    assert_eq!(rows[1].fix, Fix::TestNotification);
    assert_eq!(rows[2].fix, Fix::None);
}

/// An empty machine: what is missing says why, with one copyable install
/// command for this distribution; the top bar row is only on GNOME, and
/// Continue is never blocked (nothing here is a gate).
#[test]
fn missing_pieces_say_why_and_offer_the_install_command() {
    let checks = evaluate(&Probe {
        desktop: "sway".into(),
        wayland_display: true,
        session_bus: true,
        config_writable: true,
        ..Probe::default()
    });
    let rows = setup_rows(&SetupFacts {
        checks: &checks,
        start_at_login: false,
        extension: None,
        gnome: false,
        distro: Some(Distro::Fedora),
    });
    assert!(!ids(&rows).contains(&"extension"), "no top bar off GNOME");
    let tray = rows.iter().find(|row| row.id == "tray").unwrap();
    assert_eq!(tray.status, Status::Idle, "no tray host on this machine");
    assert!(
        tray.detail.contains("Waybar"),
        "sway's own step: {}",
        tray.detail
    );
    let plugins = rows.iter().find(|row| row.id == "plugins").unwrap();
    assert_eq!(plugins.status, Status::Err);
    let Fix::Copy(command) = &plugins.fix else {
        panic!("{:?}", plugins.fix)
    };
    assert!(command.starts_with("sudo dnf install gstreamer1-plugins-bad-free"));
    assert!(plugins.detail.contains(super::super::INSTALL_HINT));
    assert!(
        !plugins.detail.contains("sudo"),
        "the command is behind Copy command"
    );
    let autostart = &rows[0];
    assert_eq!(
        autostart.status,
        Status::Warn,
        "sway runs no autostart entries"
    );
    assert!(autostart.detail.contains("exec desklink"));
    let notifications = rows.iter().find(|row| row.id == "notifications").unwrap();
    assert!(matches!(notifications.fix, Fix::Copy(_)));
    let glide = rows.iter().find(|row| row.id == "glide").unwrap();
    assert_eq!(glide.status, Status::Err);
    assert!(glide
        .detail
        .starts_with("Move your mouse across to the phone"));
}

#[test]
fn the_top_bar_row_offers_the_one_next_step() {
    let checks = evaluate(&gnome_wayland());
    let row = |extension| {
        setup_rows(&SetupFacts {
            checks: &checks,
            start_at_login: false,
            extension: Some(extension),
            gnome: true,
            distro: None,
        })
        .into_iter()
        .find(|row| row.id == "extension")
        .unwrap()
    };
    assert_eq!(row(ExtensionStatus::Off).fix, Fix::TurnOnExtension);
    assert!(row(ExtensionStatus::Off)
        .detail
        .contains("Nothing changes until you click"));
    assert_eq!(row(ExtensionStatus::NeedsLogin).fix, Fix::LogOut);
    assert_eq!(row(ExtensionStatus::Active).fix, Fix::None);
}

// ---- Widget tests (on the dedicated test display) -----------------------

use super::super::DesklinkWindow;
use adw::subclass::prelude::ObjectSubclassIsExt;

fn window() -> (
    DesklinkWindow,
    std::sync::mpsc::Receiver<crate::device_links::daemon::DaemonCommand>,
) {
    window_in(crate::device_links::daemon::handle::LOAD_KNOWN)
}

/// A window whose daemon says `state` about the paired phones from the start.
fn window_in(
    state: u8,
) -> (
    DesklinkWindow,
    std::sync::mpsc::Receiver<crate::device_links::daemon::DaemonCommand>,
) {
    let resources = crate::load_resources().expect("the bundled resources");
    gio::resources_register(&resources);
    let app = adw::Application::builder()
        .application_id("derx06.desklink.com.test")
        .build();
    let window = DesklinkWindow::new(&app);
    let (daemon, commands) = DaemonHandle::detached();
    daemon.set_load_state_for_tests(state);
    window.set_daemon(daemon);
    (window, commands)
}

fn visible(window: &DesklinkWindow) -> String {
    window
        .imp()
        .main_stack
        .visible_child_name()
        .map(|name| name.to_string())
        .unwrap_or_default()
}

/// A fresh config with no phone: the tour is what the window shows.
/// With a paired phone: the phones, never the tour.
#[test]
fn the_tour_shows_on_a_fresh_config_and_never_with_a_paired_phone() {
    crate::test_display::run(|| {
        let (fresh, _commands) = window();
        fresh.refresh_devices();
        assert_eq!(visible(&fresh), "onboarding");
        let tour = fresh.imp().onboarding.borrow().clone().expect("the tour");
        assert!(tour.for_pairing);
        assert_eq!(tour.step(), 0);
        assert!(!tour.back.get_visible(), "no Back on the first page");
        assert!(tour.skip.get_visible(), "Skip on the glance too");
        assert_eq!(
            tour.next.label().as_deref(),
            Some(GLANCE_START),
            "one obvious button"
        );

        let (paired, _commands) = window();
        let phone = super::super::debug_fake_device("paired", "Pixel 9");
        assert!(phone.trusted);
        paired.imp().fake_devices.borrow_mut().push(phone);
        paired.refresh_devices();
        assert!(paired.imp().onboarding.borrow().is_none());
        assert_eq!(visible(&paired), "devices");
    });
}

/// The keys and buttons walk the steps: Right and Enter go on, Left goes
/// back, Esc skips to the setup step, Continue there leaves for the
/// pairing page and says the tour was seen.
#[test]
fn keys_and_buttons_walk_the_tour_to_the_pairing_page() {
    let _held = crate::platform::TEST_REPORT
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    crate::test_display::run(|| {
        crate::platform::set_report_for_tests(evaluate(&gnome_wayland()));
        let (window, commands) = window();
        window.refresh_devices();
        let tour = window.imp().onboarding.borrow().clone().expect("the tour");
        let press = |key: gdk::Key| tour.apply(key_nav(key, false).expect("a key it uses"));
        press(gdk::Key::Right);
        assert_eq!(tour.step(), 1);
        press(gdk::Key::Return);
        assert_eq!(tour.step(), 2);
        press(gdk::Key::Left);
        assert_eq!(tour.step(), 1);
        tour.next.emit_clicked();
        assert_eq!(tour.step(), 2);
        tour.back.emit_clicked();
        assert_eq!(tour.step(), 1);
        assert!(tour.back.get_visible());
        press(gdk::Key::Escape);
        assert_eq!(tour.step(), SETUP, "Skip lands on Set up this computer");
        assert!(!tour.skip.get_visible(), "nothing left to skip");
        assert_eq!(tour.next.label().as_deref(), Some("Continue"));
        // The rows are drawn from the report, each with its status.
        let mut names = Vec::new();
        let mut stack = vec![tour.root.clone().upcast::<gtk::Widget>()];
        while let Some(widget) = stack.pop() {
            if widget.is::<gtk::ListBoxRow>() {
                names.push(widget.widget_name().to_string());
            }
            let mut child = widget.first_child();
            while let Some(next) = child {
                child = next.next_sibling();
                stack.push(next);
            }
        }
        for id in [
            "autostart",
            "notifications",
            "share",
            "glide",
            "keys",
            "plugins",
        ] {
            assert!(names.iter().any(|name| name == id), "{id} in {names:?}");
        }
        tour.next.emit_clicked();
        let context = glib::MainContext::default();
        while context.iteration(false) {}
        assert!(window.imp().onboarding.borrow().is_none());
        assert_eq!(visible(&window), "welcome", "then the QR code");
        let sent: Vec<_> = commands.try_iter().collect();
        assert!(
            sent.iter()
                .any(|command| matches!(command, DaemonCommand::SetOnboardingSeen(true))),
            "remembered as seen"
        );
        crate::platform::clear_report_for_tests();
    });
}

/// The menu's replay on a paired computer ends with Done, back to the phones.
#[test]
fn a_replay_on_a_paired_computer_ends_with_done() {
    crate::test_display::run(|| {
        let (window, _commands) = window();
        let phone = super::super::debug_fake_device("paired", "Pixel 9");
        window.imp().fake_devices.borrow_mut().push(phone);
        window.refresh_devices();
        gtk::prelude::WidgetExt::activate_action(&window, "win.welcome-tour", None)
            .expect("the menu's action");
        assert_eq!(visible(&window), "onboarding");
        let tour = window.imp().onboarding.borrow().clone().expect("the tour");
        assert!(!tour.for_pairing);
        assert_eq!(tour.step(), 0, "a replay starts at the glance");
        assert_eq!(tour.next.label().as_deref(), Some(GLANCE_START));
        tour.apply(Nav::Skip);
        assert_eq!(tour.next.label().as_deref(), Some("Done"));
        tour.apply(Nav::Next);
        let context = glib::MainContext::default();
        while context.iteration(false) {}
        assert!(window.imp().onboarding.borrow().is_none());
        assert_eq!(visible(&window), "devices");
    });
}

fn seen_sent(
    commands: &std::sync::mpsc::Receiver<crate::device_links::daemon::DaemonCommand>,
) -> bool {
    commands
        .try_iter()
        .any(|command| matches!(command, DaemonCommand::SetOnboardingSeen(true)))
}

/// Until the phones are read nothing is decided; a config that could not
/// be read never shows the tour (it would greet a paired person).
#[test]
fn the_tour_waits_for_the_phones_and_skips_an_unreadable_config() {
    use crate::device_links::daemon::handle::{LOAD_FAILED, LOAD_KNOWN, LOAD_PENDING};
    crate::test_display::run(|| {
        let (waiting, _commands) = window_in(LOAD_PENDING);
        let daemon = waiting.daemon().unwrap();
        waiting.refresh_devices();
        assert!(waiting.imp().onboarding.borrow().is_none());
        assert!(!waiting.imp().onboarding_decided.get());
        daemon.set_load_state_for_tests(LOAD_KNOWN);
        waiting.refresh_devices();
        assert_eq!(visible(&waiting), "onboarding", "decided once known");

        let (failed, _commands) = window_in(LOAD_FAILED);
        failed.refresh_devices();
        assert!(failed.imp().onboarding_decided.get());
        assert!(failed.imp().onboarding.borrow().is_none());
        assert_ne!(visible(&failed), "onboarding");
    });
}

/// A pairing request interrupts the tour for the pairing flow but leaves
/// it unseen; a phone that pairs marks it seen.
#[test]
fn only_a_paired_phone_marks_the_tour_seen() {
    crate::test_display::run(|| {
        let (window, commands) = window();
        window.refresh_devices();
        assert_eq!(visible(&window), "onboarding");
        // A phone that is only nearby (a fresh install on the same Wi-Fi)
        // has asked nothing: the tour stays.
        for kind in ["discovered", "refused"] {
            let nearby = super::super::debug_fake_device(kind, "Pixel 9");
            window.imp().fake_devices.borrow_mut().push(nearby);
            window.refresh_devices();
            assert_eq!(visible(&window), "onboarding", "{kind}: the tour stays");
            window.imp().fake_devices.borrow_mut().clear();
        }
        let request = super::super::debug_fake_device("pair-request", "Pixel 9");
        assert!(!request.trusted);
        window.imp().fake_devices.borrow_mut().push(request);
        window.refresh_devices();
        assert!(
            window.imp().onboarding.borrow().is_none(),
            "the request shows"
        );
        assert!(
            !seen_sent(&commands),
            "a stray request is not the tour seen"
        );

        let (window, commands) = window_with_tour();
        let phone = super::super::debug_fake_device("paired", "Pixel 9");
        window.imp().fake_devices.borrow_mut().push(phone);
        window.refresh_devices();
        assert!(window.imp().onboarding.borrow().is_none());
        assert!(seen_sent(&commands), "paired: done with the tour");
    });
}

fn window_with_tour() -> (
    DesklinkWindow,
    std::sync::mpsc::Receiver<crate::device_links::daemon::DaemonCommand>,
) {
    let (window, commands) = window();
    window.refresh_devices();
    assert_eq!(visible(&window), "onboarding");
    (window, commands)
}

/// A setup row's action redraws the list: the keyboard focus comes back
/// to the same control instead of falling to the window.
#[test]
fn check_again_keeps_the_keyboard_focus() {
    let _held = crate::platform::TEST_REPORT
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    crate::test_display::run(|| {
        crate::platform::set_report_for_tests(evaluate(&gnome_wayland()));
        let (window, _commands) = window_with_tour();
        window.present();
        let tour = window.imp().onboarding.borrow().clone().unwrap();
        tour.apply(Nav::Skip);
        let find_again = || {
            super::super::named_widgets(tour.root.upcast_ref(), "check-again")
                .into_iter()
                .next()
                .and_downcast::<gtk::Button>()
                .expect("Check again")
        };
        let before = find_again();
        let focus = || gtk::prelude::RootExt::focus(&window);
        before.grab_focus();
        assert_eq!(focus().as_ref(), Some(before.upcast_ref()));
        let polls = tour.poll.get();
        before.emit_clicked();
        before.emit_clicked();
        let after = find_again();
        assert_ne!(before, after, "the list was redrawn");
        assert_eq!(
            focus().as_ref(),
            Some(after.upcast_ref()),
            "the focus followed it"
        );
        // One poll per click; only the newest keeps running.
        assert_eq!(tour.poll.get(), polls + 2);
        crate::platform::clear_report_for_tests();
        window.close();
    });
}
