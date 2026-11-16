//! Tests of Home: tile columns, words, links to send, terminals and notifications cards, and the page on a real GTK display (Xvfb).

use super::*;

/// No open terminal is a question with an answer, not a dead end.
#[test]
fn hand_off_with_no_terminal_offers_to_start_one() {
    let (heading, body, start) = no_terminal_words("Pixel 9");
    assert_eq!(heading, "No terminal to hand off");
    assert!(body.starts_with("Start one now?") && body.contains("on Pixel 9"));
    assert_eq!(start, "Start a terminal");
}

#[test]
fn charging_says_when_the_phone_will_be_full() {
    assert_eq!(charging_words(None), "charging");
    assert_eq!(charging_words(Some(-1)), "charging");
    assert_eq!(charging_words(Some(0)), "charging");
    assert_eq!(
        charging_words(Some(25 * 60_000)),
        "charging, full in 25 min"
    );
    assert_eq!(charging_words(Some(30_000)), "charging, full in 1 min");
    assert_eq!(
        charging_words(Some(95 * 60_000)),
        "charging, full in 1 h 35 min"
    );
}

#[test]
fn a_typed_link_gets_a_scheme_and_text_is_not_a_link() {
    assert_eq!(
        link_to_send("gnome.org/news").as_deref(),
        Some("https://gnome.org/news")
    );
    assert_eq!(
        link_to_send(" http://10.0.0.2:8080/x ").as_deref(),
        Some("http://10.0.0.2:8080/x")
    );
    assert_eq!(link_to_send("not a link"), None);
    assert_eq!(link_to_send("localhost"), None);
    assert_eq!(link_to_send(""), None);
}

/// The same vectors as `tests/model.test.mjs` (`looksLikeLink`).
#[test]
fn only_web_addresses_and_site_names_become_links() {
    for (typed, sent) in [
        ("https://example.org/a?b=c", "https://example.org/a?b=c"),
        ("http://example.org", "http://example.org"),
        ("example.org", "https://example.org"),
        (
            "www.example.com/path?x=1",
            "https://www.example.com/path?x=1",
        ),
        ("sub.example.co.uk", "https://sub.example.co.uk"),
        ("example.org:8080/x", "https://example.org:8080/x"),
        ("HTTPS://Example.org", "HTTPS://Example.org"),
        ("http://localhost:3000", "http://localhost:3000"),
    ] {
        assert_eq!(link_to_send(typed).as_deref(), Some(sent), "{typed}");
    }
    for typed in [
        "README.md",
        "notes.txt",
        "photo.jpg",
        "foo.md/bar",
        "archive.tar.gz",
        "file:///etc/passwd",
        "ftp://example.org",
        "javascript:alert.x",
        "mailto:a@b.org",
        "data:text/plain,hi",
        "https://",
        "http:///x",
        "https://exa mple.org",
        "hello",
        "1.2.3",
        "192.168.1.1",
        "a..b.org",
        "-a.org",
        "example.org:abc",
        "example.org:",
        "https://good.com@evil.org",
        "[::1]:80",
    ] {
        assert_eq!(link_to_send(typed), None, "{typed}");
    }
}

#[test]
fn the_more_tile_names_what_is_in_it() {
    assert_eq!(
        more_label("Pixel 9", &["Lock phone", "Send a link…"]),
        "More actions for Pixel 9: Lock phone, Send a link…"
    );
}

/// Six tiles in a row, then three by two, then two by three: never one
/// alone on a line, from the width the row has and the width a tile needs
/// (the owner's 1920 px window showed five, then More alone).
#[test]
fn the_tile_row_never_leaves_an_orphan() {
    // Tiles of 140 px with 12 px gaps: six need 900, three 444.
    assert_eq!(tile_columns(900, 140), 6);
    assert_eq!(
        tile_columns(880, 140),
        3,
        "five would fit: not six, so three"
    );
    assert_eq!(tile_columns(444, 140), 3);
    assert_eq!(tile_columns(443, 140), 2);
    assert_eq!(tile_columns(200, 140), 2, "two at least: the row wraps");
    for (width, tile) in [(1000, 150), (700, 150), (300, 150), (1600, 230)] {
        assert_eq!(6 % tile_columns(width, tile), 0, "{width}");
    }
}

#[test]
fn the_live_strip_and_the_card_words_are_sentences() {
    assert_eq!(
        live_detail("viewing your screen", None),
        "Viewing your screen"
    );
    assert_eq!(
        live_detail("in a terminal, viewing your screen", Some("14:02")),
        "In a terminal, viewing your screen · since 14:02"
    );
    assert_eq!(
        live_detail("a second display", Some("")),
        "A second display"
    );
    assert_eq!(terminals_words(0, 0, "Pixel"), "0 open");
    assert_eq!(
        terminals_words(2, 1, "Pixel"),
        "2 open · 1 shared with Pixel"
    );
}

/// A tile that cannot be used says why in a word, with the same words as
/// the status pill.
#[test]
fn a_tile_that_cannot_be_used_says_why() {
    let info = crate::device_links::device_info::DeviceInfo::local("p".into(), "Pixel".into());
    let mut device = DeviceView::from_info(&info, "10.0.0.2".into(), true);
    device.status = DeviceStatus::Paired;
    device.feature_transport_ready = true;
    let reason = |device: &DeviceView| {
        blocker(
            Feature::Send(PACKET_TYPE_PING),
            &PhoneFacts::from_device(device),
        )
        .map(|need| super::super::requirements::short(&need))
    };
    device.incoming_capabilities = vec![PACKET_TYPE_PING.into()];
    assert_eq!(reason(&device), None);
    device.feature_transport_ready = false;
    assert_eq!(reason(&device), Some("Connecting…"));
    device.status = DeviceStatus::Unreachable;
    assert_eq!(reason(&device), Some("Can't reach"));
    assert_eq!(
        Presentation::of(&device).word,
        reason(&device).unwrap(),
        "the pill and the tile agree"
    );
}

/// The hero's name is two lines at most, then an ellipsis, and a heading
/// for screen readers. (Needs a display.)
#[test]
fn a_long_name_is_two_lines_then_cut() {
    crate::test_display::run(|| {
        let name = hero_name(&"Aman's Pixel 9 Pro XL Fold ".repeat(6));
        assert_eq!(name.lines(), 2);
        assert_eq!(name.ellipsize(), gtk::pango::EllipsizeMode::End);
        assert!(name.wraps() && name.has_css_class("dl-hero"));
        assert_eq!(name.accessible_role(), gtk::AccessibleRole::Heading);
    });
}

/// Every label under `root`, and the widgets carrying `class`.
fn words_and(root: &gtk::Widget, class: &str) -> (Vec<String>, Vec<gtk::Widget>) {
    let all = named_widgets(root, "");
    let words = all
        .iter()
        .filter_map(|w| w.downcast_ref::<gtk::Label>())
        .map(|label| label.label().to_string())
        .collect();
    let marked = all.into_iter().filter(|w| w.has_css_class(class)).collect();
    (words, marked)
}

/// Away for hours: "Can't reach" is said once (the pill and one line), with
/// one Try again and no stale "dropped while it was being set up"; the
/// hero's two buttons are left out (nothing to send to), the battery reads
/// as old, and a tablet is drawn as one. (Needs a display.)
#[test]
fn an_away_tablet_says_so_once_and_nothing_reads_as_live() {
    crate::test_display::run(|| {
        let info =
            crate::device_links::device_info::DeviceInfo::local("t".into(), "emulator".into());
        let mut device = DeviceView::from_info(&info, "10.0.0.2".into(), true);
        device.device_type = "tablet".into();
        device.status = DeviceStatus::Unreachable;
        device.last_seen = Some(1_700_000_000);
        device.battery_status = Some(crate::device_links::device::BatteryStatus {
            current_charge: 100,
            is_charging: false,
            battery_quantity: None,
            threshold_event: None,
            charge_time_remaining: None,
        });
        let (daemon, _commands) = crate::device_links::daemon::DaemonHandle::detached();
        let once = |device: &DeviceView| {
            let band = hero(&daemon, device, "17:07");
            let root = band.upcast_ref::<gtk::Widget>().clone();
            let (words, _) = words_and(&root, "");
            let reach = words.iter().filter(|w| w.contains("Can't reach")).count();
            let retries = named_widgets(&root, "hero-reconnect").len();
            (root, words, reach, retries)
        };

        let (root, words, reach, retries) = once(&device);
        assert_eq!((reach, retries), (1, 1), "{words:?}");
        assert!(!words.iter().any(|w| w.contains("being set up")));
        assert!(
            named_widgets(&root, "hero-actions").is_empty(),
            "no greyed Send files and Send clipboard: nothing to send them to"
        );
        let (_, stale) = words_and(&root, "dl-stale");
        assert_eq!(stale.len(), 2, "the ring and its percent");
        let (_, art) = words_and(&root, "phone-art");
        assert!(art[0].has_css_class("tablet"));
        assert!(art[0].width_request() > art[0].height_request());

        // The ladder's own "Can't reach … · Try again" adds nothing more.
        device.last_error = Some(crate::device_links::webrtc::coordinator::unavailable_words(
            "emulator",
        ));
        let (_, words, reach, retries) = once(&device);
        assert_eq!((reach, retries), (1, 1), "{words:?}");

        // Live: no Try again, nothing dimmed, and a phone is a phone.
        device.last_error = None;
        device.device_type = "phone".into();
        device.status = DeviceStatus::Paired;
        device.feature_transport_ready = true;
        device.last_seen = None;
        device.incoming_capabilities = vec![
            PACKET_TYPE_SHARE_REQUEST.into(),
            crate::device_links::packet::PACKET_TYPE_CLIPBOARD.into(),
        ];
        let (root, _, reach, retries) = once(&device);
        assert_eq!((reach, retries), (0, 0));
        assert!(words_and(&root, "dl-unavailable").1.is_empty());
        assert!(words_and(&root, "dl-stale").1.is_empty());
        let (_, art) = words_and(&root, "phone-art");
        assert!(!art[0].has_css_class("tablet"));
    });
}

#[test]
fn a_long_copied_line_is_cut_short() {
    let long = "x".repeat(500);
    let shown = preview_line(&format!("{long}\nsecond"));
    assert_eq!(shown.chars().count(), 201);
    assert!(shown.ends_with('…'));
    assert_eq!(preview_line("short\nmore"), "short");
}

/// A window showing Home for a fake phone in `state`, `width` px wide.
fn home_window(state: &str, width: i32) -> super::super::DesklinkWindow {
    home_window_of(state, width, |_| {})
}

/// A paired fake phone, changed by `change` first.
fn home_window_with(
    width: i32,
    change: impl FnOnce(&mut DeviceView),
) -> super::super::DesklinkWindow {
    home_window_of("paired", width, change)
}

fn home_window_of(
    state: &str,
    width: i32,
    change: impl FnOnce(&mut DeviceView),
) -> super::super::DesklinkWindow {
    use adw::subclass::prelude::ObjectSubclassIsExt;
    let resources = crate::load_resources().expect("the bundled resources");
    gtk::gio::resources_register(&resources);
    let app = adw::Application::builder()
        .application_id("derx06.desklink.com.test")
        .build();
    let window = super::super::DesklinkWindow::new(&app);
    let (daemon, _commands) = DaemonHandle::detached();
    window.set_daemon(daemon);
    let mut phone = super::super::debug_fake_device(state, "Pixel 9");
    change(&mut phone);
    let id = phone.id.clone();
    window.imp().fake_devices.borrow_mut().push(phone);
    window.select_device(&id);
    // A narrow window shows the sidebar first: the page, as after a click.
    window.imp().split_view.set_show_content(true);
    window.set_default_size(width, 1000);
    window.present();
    let context = gtk::glib::MainContext::default();
    for _ in 0..60 {
        while context.iteration(false) {}
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    window.refresh_devices();
    for _ in 0..60 {
        while context.iteration(false) {}
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    window
}

fn shown(window: &super::super::DesklinkWindow, name: &str) -> Option<gtk::Widget> {
    named_widgets(window.upcast_ref(), name)
        .into_iter()
        .find(|widget| widget.is_mapped())
}

/// A big window shows Home in two columns, the phone and its actions on the
/// left and what needs a look on the right; below the breakpoint, one
/// column in reading order. At every width the page fits: nothing is wider
/// than the window (the owner's 1920 px window had 400 px empty each side
/// and tiles five, then More alone). The test display is 1280 px wide, so
/// the two-column state is set as the breakpoint sets it, and the
/// breakpoint's own condition is checked. (Needs a display.)
#[test]
fn home_uses_a_big_window_and_fits_every_width() {
    use adw::subclass::prelude::ObjectSubclassIsExt;
    crate::test_display::run(|| {
        let check = |window: &super::super::DesklinkWindow, width: i32, two: bool| {
            let split = shown(window, "home-split").expect("Home's columns");
            let split = split.downcast::<gtk::Box>().unwrap();
            let horizontal = split.orientation() == gtk::Orientation::Horizontal;
            assert_eq!(horizontal, two, "{width} px: two columns");
            let left = split.first_child().expect("the left column");
            assert!(
                shown(window, "tile-row")
                    .is_some_and(|tiles| tiles.parent().as_ref() == Some(&left)),
                "{width} px: the actions are with the phone"
            );
            let (min, _, _, _) = split.measure(gtk::Orientation::Horizontal, -1);
            assert!(min <= width, "{width} px: Home needs {min} px");
            if let Some(tiles) = shown(window, "tile-row").and_downcast::<gtk::FlowBox>() {
                let columns = tiles.max_children_per_line();
                assert_eq!(
                    6 % columns,
                    0,
                    "{width} px: {columns} tiles a row, no orphan"
                );
            }
        };
        for width in [360, 800, 1100] {
            let window = home_window("paired", width);
            check(&window, width, false);
            window.close();
        }
        // The page area at the breakpoint: 1200 px (a window of about 1500).
        let window = home_window("paired", 1280);
        let imp = window.imp();
        assert_eq!(
            imp.wide_breakpoint
                .condition()
                .map(|c| c.to_str().to_string())
                .as_deref(),
            Some("min-width: 1200sp")
        );
        imp.compact.set(false);
        imp.wide.set(true);
        window.apply_layout();
        check(&window, 1200, true);
        let clamp = shown(&window, "home-clamp")
            .and_downcast::<adw::Clamp>()
            .unwrap();
        assert_eq!(clamp.maximum_size(), HOME_WIDE_WIDTH);
        imp.wide.set(false);
        window.apply_layout();
        check(&window, 1200, false);
        assert_eq!(clamp.maximum_size(), HOME_WIDTH);
        window.close();
    });
}

/// Away, Home is the hero and what came in: no row of tiles that each say
/// "Can't reach", no setup steps that need the phone, no feature list
/// repeating it; Try again is there once. (Needs a display.)
#[test]
fn an_away_phone_gets_a_calm_home() {
    crate::test_display::run(|| {
        let window = home_window("unreachable", 1920);
        assert!(shown(&window, "tile-row").is_none(), "no tiles");
        assert!(shown(&window, "finish-setup").is_none(), "no setup steps");
        assert!(shown(&window, "everything").is_none(), "no feature list");
        assert_eq!(
            named_widgets(window.upcast_ref(), "hero-reconnect")
                .iter()
                .filter(|widget| widget.is_mapped())
                .count(),
            1,
            "one Try again"
        );
        window.close();

        let window = home_window("paired", 1920);
        assert!(shown(&window, "tile-row").is_some(), "back: the tiles");
        assert!(shown(&window, "everything").is_some(), "back: the list");
        window.close();
    });
}

fn spin() {
    let context = gtk::glib::MainContext::default();
    for _ in 0..60 {
        while context.iteration(false) {}
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

/// The real breakpoint, crossed by resizing the window both ways, and the
/// tile row re-measured from the width its column then has (read after the
/// main loop ran, not before the new allocation). The test display is 1280
/// px wide, so the text scale is 0.75: 1200sp is then 900 px. (Needs a
/// display.)
#[test]
fn home_crosses_the_breakpoint_when_the_window_is_resized() {
    use adw::subclass::prelude::ObjectSubclassIsExt;
    crate::test_display::run(|| {
        let settings = gtk::Settings::default().expect("GTK settings");
        let dpi = settings.gtk_xft_dpi();
        settings.set_gtk_xft_dpi((96.0 * 1024.0 * 0.75) as i32);
        let window = home_window("paired", 1280);
        let tiles_fit = |window: &super::super::DesklinkWindow| {
            let tiles = shown(window, "tile-row")
                .and_downcast::<gtk::FlowBox>()
                .expect("the tile row");
            let column = tiles.parent().expect("its column").width();
            let tile = tiles
                .first_child()
                .map(|child| child.measure(gtk::Orientation::Horizontal, -1).1)
                .unwrap();
            assert_eq!(
                tiles.max_children_per_line(),
                tile_columns(column, tile),
                "columns from the width the row has now ({column} px)"
            );
        };
        let columns = |window: &super::super::DesklinkWindow| {
            shown(window, "home-split")
                .and_downcast::<gtk::Box>()
                .unwrap()
                .orientation()
        };
        for (width, wide) in [(1280, true), (700, false), (1280, true)] {
            window.set_visible(false);
            window.set_default_size(width, 1000);
            window.present();
            spin();
            assert_eq!(window.imp().wide.get(), wide, "{width} px");
            let expected = if wide {
                gtk::Orientation::Horizontal
            } else {
                gtk::Orientation::Vertical
            };
            assert_eq!(columns(&window), expected, "{width} px");
            tiles_fit(&window);
        }
        window.close();
        settings.set_gtk_xft_dpi(dpi);
    });
}

/// The reconnect ladder gave up while the phone still reads as paired: the
/// same calm Home as away (the owner's screen had six greyed tiles, the
/// setup steps and the feature list under "Can't reach · Try again"). A
/// phone still connecting keeps its tiles. (Needs a display.)
#[test]
fn a_phone_the_ladder_gave_up_on_is_away_too() {
    crate::test_display::run(|| {
        let mut device = super::super::debug_fake_device("paired", "Pixel 9");
        device.feature_transport_ready = false;
        assert!(!phone_away(&device), "connecting is not away");
        device.last_error = Some(crate::device_links::webrtc::coordinator::unavailable_words(
            "Pixel 9",
        ));
        assert!(phone_away(&device), "the ladder gave up");
        device.last_error = None;
        device.status = DeviceStatus::Unreachable;
        assert!(phone_away(&device));

        let window = home_window_with(1100, |device| {
            device.feature_transport_ready = false;
            device.last_error = Some(crate::device_links::webrtc::coordinator::unavailable_words(
                &device.name,
            ));
        });
        assert!(shown(&window, "tile-row").is_none(), "no tiles");
        assert!(shown(&window, "finish-setup").is_none(), "no setup steps");
        assert!(shown(&window, "everything").is_none(), "no feature list");
        assert!(shown(&window, "hero-actions").is_none(), "no greyed Send");
        window.close();

        let window = home_window_with(1100, |device| device.feature_transport_ready = false);
        assert!(
            shown(&window, "tile-row").is_some(),
            "connecting: the tiles stay"
        );
        window.close();
    });
}

/// The other pages on a big window use the width for structure: Files and
/// the Bucket wider with eight cards a line, the Notes list wider beside the
/// note, four terminals a line; back to their usual layout below the
/// breakpoint. A page shown while the window is already big is laid out for
/// it at once (show_page runs the layout pass: nothing else is called here).
/// (Needs a display.)
#[test]
fn the_other_pages_use_a_big_window() {
    use adw::subclass::prelude::ObjectSubclassIsExt;
    crate::test_display::run(|| {
        let window = home_window("paired", 1280);
        let imp = window.imp();
        let notes = || {
            named_widgets(window.upcast_ref(), "notes-split")
                .into_iter()
                .next()
                .and_downcast::<adw::NavigationSplitView>()
                .expect("the Notes split")
        };
        let check = |wide: bool| {
            imp.compact.set(false);
            imp.wide.set(wide);
            // Only the pages being shown: show_page lays them out.
            for page in ["files", "terminals", "contacts", "notes"] {
                assert!(window.show_page(page), "{page}");
                spin();
            }
            for clamp in named_widgets(window.upcast_ref(), "files-clamp") {
                let clamp = clamp.downcast::<adw::Clamp>().unwrap();
                assert_eq!(
                    clamp.maximum_size(),
                    if wide {
                        super::super::FILES_WIDE_WIDTH
                    } else {
                        super::super::FILES_WIDTH
                    }
                );
            }
            for grid in named_widgets(window.upcast_ref(), "card-grid") {
                let grid = grid.downcast::<gtk::FlowBox>().unwrap();
                assert_eq!(grid.max_children_per_line(), if wide { 8 } else { 5 });
            }
            // The Bucket's grid takes the same rule, from 6.
            assert_eq!(window.card_columns(6), if wide { 8 } else { 6 });
            assert_eq!(window.card_columns(5), if wide { 8 } else { 5 });
            let split = notes();
            assert_eq!(split.max_sidebar_width(), if wide { 420.0 } else { 320.0 });
            assert_eq!(
                split.sidebar_width_fraction(),
                if wide { 0.32 } else { 0.25 }
            );
            for grid in named_widgets(window.upcast_ref(), "terminal-grid") {
                let grid = grid.downcast::<gtk::FlowBox>().unwrap();
                assert_eq!(grid.max_children_per_line(), if wide { 4 } else { 3 });
            }
            // Contacts: the chosen contact beside the list on a big window.
            for pane in named_widgets(window.upcast_ref(), "contacts-pane") {
                assert_eq!(pane.is_visible(), wide, "the contact pane");
            }
            // The one-a-line list of moving files is not a card grid.
            for moving in named_widgets(window.upcast_ref(), "moving") {
                let moving = moving.downcast::<gtk::FlowBox>().unwrap();
                assert_eq!(moving.max_children_per_line(), 1);
            }
        };
        check(true);
        check(false);
        check(true);
        assert!(
            !named_widgets(window.upcast_ref(), "files-clamp").is_empty(),
            "Files was drawn"
        );
        window.close();
    });
}
