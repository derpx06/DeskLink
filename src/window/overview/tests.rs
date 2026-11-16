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
/// alone on a line.
#[test]
fn the_tile_row_never_leaves_an_orphan() {
    assert_eq!(tile_columns(1000), 6);
    assert_eq!(tile_columns(840), 6);
    assert_eq!(tile_columns(839), 3);
    assert_eq!(tile_columns(560), 3);
    assert_eq!(tile_columns(559), 2);
    assert_eq!(tile_columns(280), 2);
    // 6 tiles divide into rows of 6, 3 and 2.
    for width in [1000, 700, 300] {
        assert_eq!(6 % tile_columns(width), 0, "{width}");
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
/// hero's two buttons are unavailable like the tiles, the battery reads as
/// old, and a tablet is drawn as one. (Needs a display.)
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
        let actions = named_widgets(&root, "hero-actions");
        let (_, dimmed) = words_and(&actions[0], "dl-unavailable");
        assert_eq!(dimmed.len(), 2, "Send files and Send clipboard");
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
