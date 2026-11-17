//! The popup's words and rows, and the rules that keep update UI where the
//! owner wants it: a popup and Settings controls only.

use super::model::*;
use super::words;
use crate::update::{Component, Family, Glyph, Guidance, Note, Source, Version};
use adw::prelude::*;

fn component(notes: usize) -> Component {
    Component {
        version: Version(0, 4, 0),
        released: None,
        min_supported: Version(0, 0, 0),
        required_reason: None,
        rollout: 100,
        notes: (0..notes)
            .map(|i| Note {
                title: format!("Note {i}"),
                detail: (i % 2 == 0).then(|| format!("Detail {i}")),
                glyph: Glyph::Files,
            })
            .collect(),
        notes_url: Some("https://github.com/derpx06/DeskLink/releases/tag/v0.4.0".into()),
        release_url: None,
        tarball: None,
    }
}

#[test]
fn the_title_chips_and_notes_follow_the_offer() {
    let p = popup(
        &component(5),
        Version(0, 3, 0),
        true,
        false,
        Source::Package(Family::Debian),
    );
    assert_eq!(p.title, "DeskLink 0.4.0 (beta) is available");
    assert_eq!(
        (p.from.as_str(), p.to.as_str()),
        ("0.3.0 (beta)", "0.4.0 (beta)")
    );
    assert_eq!(p.notes.len(), 3, "the first three");
    assert_eq!(p.notes[0].detail.as_deref(), Some("Detail 0"));
    assert_eq!(p.notes[1].detail, None);
    assert_eq!(p.installed_with, "Installed with Debian or Ubuntu package");
    assert!(p.body.is_none() && !p.required);
    let stable = popup(
        &component(0),
        Version(1, 0, 0),
        false,
        false,
        Source::Manual,
    );
    assert_eq!(stable.title, "DeskLink 0.4.0 is available");
    assert_eq!(stable.from, "1.0.0");
    assert_eq!(stable.installed_with, "Installed with a download");
    assert!(stable.notes.is_empty());
}

#[test]
fn the_required_popup_has_a_reason_and_no_notes() {
    let mut c = component(3);
    let p = popup(&c, Version(0, 2, 0), true, true, Source::Flatpak);
    assert_eq!(p.title, "Update needed");
    assert_eq!(p.body.as_deref(), Some(words::REQUIRED_BODY));
    c.required_reason = Some("This version cannot connect to DeskLink 1.4.".into());
    let p = popup(&c, Version(0, 2, 0), true, true, Source::Flatpak);
    assert_eq!(
        p.body.as_deref(),
        Some("This version cannot connect to DeskLink 1.4.")
    );
}

#[test]
fn the_guidance_is_the_documented_one_per_source() {
    let g = |s: Source| guide(&popup(&component(0), Version(0, 3, 0), true, false, s));
    let debian = g(Source::Package(Family::Debian));
    assert_eq!(
        debian.command,
        Some((
            "sudo apt update && sudo apt install --only-upgrade desklink",
            None
        ))
    );
    assert_eq!(
        debian.lag.as_deref(),
        Some("Your distribution may not have 0.4.0 yet.")
    );
    assert_eq!(
        g(Source::Flatpak).command.map(|c| c.0),
        Some("flatpak update derx06.desklink.com")
    );
    let manual = g(Source::Manual);
    assert!(manual.command.is_none() && manual.open_page && manual.lag.is_none());
    let appimage = g(Source::AppImage);
    assert!(appimage.text.unwrap().contains("can't update itself") && appimage.open_page);
    let nix = g(Source::Nix);
    assert!(nix.text.is_some() && !nix.open_page);
    assert!(matches!(Source::Nix.guidance(), Guidance::Text(_)));
    // The release page is the manifest's, else the repository's.
    let p = popup(&component(0), Version(0, 3, 0), true, false, Source::Manual);
    assert!(p.release_url.as_deref().unwrap().ends_with("/v0.4.0"));
    assert!(RELEASES_PAGE.starts_with("https://github.com/derpx06/DeskLink/"));
}

#[test]
fn every_note_glyph_has_an_icon_from_the_bundle() {
    let bundled = include_str!("../../desklink.gresource.xml");
    for glyph in [
        Glyph::Refresh,
        Glyph::Swap,
        Glyph::Check,
        Glyph::Files,
        Glyph::Terminal,
        Glyph::Speed,
        Glyph::Shield,
        Glyph::Star,
    ] {
        let icon = glyph_icon(glyph);
        assert!(bundled.contains(&format!("{icon}.svg")), "{icon}");
    }
}

#[test]
fn the_words_are_sentence_case_in_computer_and_phone_vocabulary() {
    let all = [
        words::HOW_TO_UPDATE,
        words::REMIND_TOMORROW,
        words::SKIP_VERSION,
        words::DONE,
        words::COPY_COMMAND,
        words::COMMAND_COPIED,
        words::TO_UPDATE_RUN,
        words::NEVER_BY_ITSELF,
        words::RELEASE_NOTES,
        words::OPEN_DOWNLOAD_PAGE,
        words::REQUIRED_TITLE,
        words::REQUIRED_BODY,
        words::CHECK_ROW,
        words::CHECKING,
        words::UP_TO_DATE,
        words::CHECK_FAILED,
        words::AUTO_ROW,
        words::AUTO_SUB,
        words::AUTO_OFF_SUB,
        words::CHANNEL_ROW,
        words::STABLE_LATER,
        words::NOT_SET_UP,
        words::SHARE_NOTE,
    ];
    for text in all {
        for banned in [
            "device",
            "handset",
            "laptop",
            "PC",
            "Settings > ",
            "server error",
        ] {
            // "your device ID" is the one place the word is the thing named.
            if text == words::SHARE_NOTE && banned == "device" {
                continue;
            }
            assert!(!text.contains(banned), "{text:?} says {banned:?}");
        }
        let first = text.chars().next().unwrap();
        assert!(first.is_uppercase(), "{text:?}");
    }
    assert!(words::NEVER_BY_ITSELF.contains("never downloads or installs"));
    assert_eq!(
        words::skew_older("0.2.0"),
        "Runs DeskLink 0.2.0. Update it for the newest features."
    );
    assert_eq!(
        words::skew_newer("0.9.0"),
        "Runs a newer DeskLink (0.9.0). Update this computer."
    );
}

/// The owner's rule: the update is a popup and Settings controls. No update
/// widget in the Home overview, the sidebar or a list, no notification, and
/// all manifest text drawn as plain text.
#[test]
fn update_ui_lives_only_in_the_popup_and_settings() {
    let overview = [
        include_str!("../overview/balance.rs"),
        include_str!("../overview/cards.rs"),
        include_str!("../overview/tiles.rs"),
        include_str!("../sidebar.rs"),
    ];
    for source in overview {
        let code = crate::source_scan::production(source);
        assert!(
            !code.contains("window::updates") && !code.contains("crate::update"),
            "an update widget outside the popup and Settings"
        );
    }
    for source in [
        include_str!("dialog.rs"),
        include_str!("coordinator.rs"),
        include_str!("settings.rs"),
        include_str!("illustration.rs"),
    ] {
        let code = crate::source_scan::production(source);
        for word in [
            "desk_notification",
            "send_notification",
            "Notification::new",
            "use_markup(true)",
            "set_markup",
            "glib::timeout_add_local_full",
        ] {
            assert!(!code.contains(word), "{word}");
        }
    }
    // Manifest text goes through labels that never parse markup.
    let dialog = crate::source_scan::production(include_str!("dialog.rs"));
    assert!(dialog.contains(".use_markup(false)"));
    assert!(!dialog.contains("Label::new(") && !dialog.contains("with_mnemonic"));
}

/// The popup is shown only from an open: the coordinator has two entry
/// points, and nothing in the module starts a recurring timer that shows it.
#[test]
fn the_popup_is_shown_only_from_an_open_or_a_request() {
    let coordinator = crate::source_scan::production(include_str!("coordinator.rs"));
    assert_eq!(coordinator.matches("pub(crate) fn ").count(), 2);
    assert!(!coordinator.contains("timeout_add_seconds"));
    // Only a refresh at an open and an explicit request poll, and both are bounded.
    assert!(coordinator.contains("OPEN_BUDGET_MS") && coordinator.contains("FORCED_WAIT_SECS"));
    // The only caller of the open is the window being shown again.
    let application = crate::source_scan::production(include_str!("../../application/mod.rs"));
    assert_eq!(application.matches("on_window_presented").count(), 1);
}

/// Without an update address in the build (or before the service started),
/// Settings shows the one line saying so and no control.
#[test]
fn settings_without_an_update_service_show_only_the_not_set_up_line() {
    crate::test_display::run(|| {
        let (daemon, _commands) = crate::device_links::daemon::DaemonHandle::detached();
        let settings = crate::device_links::config::AppSettings::default();
        let rows = super::settings::updates_rows(&daemon, &settings, || None);
        assert_eq!(rows.len(), 1);
        let row = rows[0].downcast_ref::<adw::ActionRow>().unwrap();
        assert_eq!(row.title(), words::GROUP_TITLE);
        assert_eq!(row.subtitle().unwrap(), words::NOT_SET_UP);
        assert!(super::is_update_row(&row.title()));
    });
}

/// The one muted line under a phone's name: only for a different minor.
#[test]
fn the_skew_line_follows_the_phones_version() {
    use crate::device_links::device::DeviceView;
    use crate::device_links::device_info::DeviceInfo;
    let device = |version: Option<&str>| {
        let info = DeviceInfo {
            id: "0123456789abcdef0123456789abcdef".to_string(),
            name: "Pixel".to_string(),
            device_type: "phone".to_string(),
            protocol_version: 9,
            incoming_capabilities: Vec::new(),
            outgoing_capabilities: Vec::new(),
            instance_id: None,
            app_version: None,
        };
        let mut view = DeviceView::from_info(&info, String::new(), true);
        view.app_version = version.map(str::to_string);
        view
    };
    let ours = crate::update::Version::parse(crate::config::VERSION).unwrap();
    let older = format!("{}.{}.0", ours.0, ours.1.saturating_sub(1));
    if ours.1 > 0 {
        assert!(super::skew_line(&device(Some(&older)))
            .unwrap()
            .starts_with("Runs DeskLink"));
    }
    let newer = format!("{}.{}.0", ours.0, ours.1 + 1);
    assert!(super::skew_line(&device(Some(&newer)))
        .unwrap()
        .starts_with("Runs a newer DeskLink"));
    assert_eq!(super::skew_line(&device(Some(&ours.to_string()))), None);
    assert_eq!(super::skew_line(&device(None)), None);
    assert_eq!(super::skew_line(&device(Some("garbage"))), None);
}

/// The desk-only controls are exactly these five titles (the contract test
/// lets them through), and they are the words the rows really use.
#[test]
fn the_desk_only_rows_are_pinned() {
    assert_eq!(
        super::DESK_ONLY_ROWS,
        [
            "Updates",
            "Check for updates",
            "Check automatically",
            "Update channel",
            "What a check sends"
        ]
    );
    assert_eq!(words::GROUP_TITLE, "Updates");
    assert_eq!(words::CHECK_ROW, "Check for updates");
    assert_eq!(words::AUTO_ROW, "Check automatically");
    assert_eq!(words::CHANNEL_ROW, "Update channel");
    assert_eq!(words::WHAT_A_CHECK_SENDS, "What a check sends");
    assert!(!super::is_update_row("Skipped 0.4.0"));
}

// ---- the open state, through the coordinator -------------------------------

mod open_state {
    use super::super::coordinator::{State, Step};
    use crate::update::{Channel, Component, Decision, Snapshot, Status, Version};

    fn offer() -> Component {
        Component {
            version: Version(0, 4, 0),
            released: None,
            min_supported: Version(0, 0, 0),
            required_reason: None,
            rollout: 100,
            notes: vec![],
            notes_url: None,
            release_url: None,
            tarball: None,
        }
    }

    fn snapshot(status: Status, decision: Decision, age: Option<u64>) -> Snapshot {
        Snapshot {
            status,
            last_check: Status::Idle,
            manual_decision: decision.clone(),
            decision,
            installed: Version(0, 3, 0),
            channel: Channel::Beta,
            last_checked: 1,
            cache_age: age,
            skipped: None,
            generation: 0,
        }
    }

    #[test]
    fn each_open_retires_the_pollers_of_the_one_before() {
        let mut state = State::default();
        let fresh = snapshot(Status::Idle, Decision::Available(offer()), Some(10));
        let (first, step) = state.open(&fresh, false);
        assert_eq!(step, Step::Show { required: false });
        assert!(state.is_current(first));
        // The window is hidden and shown again: a new open.
        let (second, _) = state.open(&fresh, false);
        assert_ne!(first, second);
        assert!(
            !state.is_current(first),
            "the first open's poller is stale now"
        );
        assert!(state.is_current(second));
        // A slow answer for the first open shows nothing, whatever it says.
        let decision = Decision::Available(offer());
        assert_eq!(
            state.settled(first, true, false, Some(100), &decision),
            Step::Nothing
        );
        assert_eq!(
            state.settled(second, true, false, Some(100), &decision),
            Step::Show { required: false }
        );
    }

    #[test]
    fn the_same_open_asked_again_does_nothing() {
        let mut state = State::default();
        let fresh = snapshot(Status::Idle, Decision::Available(offer()), Some(10));
        let (id, _) = state.open(&fresh, false);
        assert_eq!(
            state.again(id, &fresh, false),
            Step::Nothing,
            "claimed before it was planned"
        );
    }

    #[test]
    fn a_hidden_window_or_a_busy_screen_or_a_late_answer_show_nothing() {
        let mut state = State::default();
        let stale = snapshot(Status::Idle, Decision::UpToDate, Some(999_999));
        let (id, step) = state.open(&stale, false);
        assert_eq!(step, Step::Refresh);
        let found = Decision::Available(offer());
        assert_eq!(
            state.settled(id, false, false, Some(100), &found),
            Step::Nothing,
            "hidden again"
        );
        assert_eq!(
            state.settled(id, true, true, Some(100), &found),
            Step::Nothing,
            "another dialog is up"
        );
        assert_eq!(
            state.settled(id, true, false, Some(3_501), &found),
            Step::Nothing,
            "late"
        );
        assert_eq!(
            state.settled(id, true, false, None, &found),
            Step::Show { required: false },
            "failed: the cache stands"
        );
        assert_eq!(
            state.settled(id, true, false, Some(3_400), &found),
            Step::Show { required: false }
        );
    }

    #[test]
    fn a_required_update_shows_with_checks_off_and_never_touches_the_network() {
        let mut state = State::default();
        let required = Decision::Required(offer());
        // Checks off: the cached Required shows, and no refresh is planned
        // even though the cache is old.
        let (_, step) = state.open(
            &snapshot(Status::Disabled, required.clone(), Some(9_999_999)),
            false,
        );
        assert_eq!(step, Step::Show { required: true });
        // An ordinary offer does not, with checks off.
        let (_, step) = state.open(
            &snapshot(Status::Disabled, Decision::Available(offer()), Some(10)),
            false,
        );
        assert_eq!(step, Step::Nothing);
        let (_, step) = state.open(&snapshot(Status::Disabled, Decision::UpToDate, None), false);
        assert_eq!(step, Step::Nothing, "and no refresh");
        // No update address in this build: nothing at all.
        let (_, step) = state.open(&snapshot(Status::NotConfigured, required, Some(1)), false);
        assert_eq!(step, Step::Nothing);
    }

    #[test]
    fn a_busy_screen_skips_the_open() {
        let mut state = State::default();
        let (_, step) = state.open(
            &snapshot(Status::Idle, Decision::Available(offer()), Some(10)),
            true,
        );
        assert_eq!(step, Step::Nothing);
    }
}

// ---- the buttons, on a real dialog (needs a display) -----------------------

mod clicks {
    use super::super::coordinator::{blocked, present_popup};
    use super::super::dialog::Sink;
    use super::super::model::popup;
    use crate::update::{Command, Component, Family, Source, Version};
    use crate::window::DesklinkWindow;
    use adw::prelude::*;
    use gtk::{gio, glib};
    use std::cell::RefCell;
    use std::rc::Rc;

    fn offer() -> Component {
        Component {
            version: Version(0, 4, 0),
            released: None,
            min_supported: Version(0, 0, 0),
            required_reason: None,
            rollout: 100,
            notes: vec![],
            notes_url: None,
            release_url: None,
            tarball: None,
        }
    }

    fn pump() {
        let context = glib::MainContext::default();
        for _ in 0..30 {
            while context.iteration(false) {}
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    fn walk(widget: &gtk::Widget, found: &mut Vec<gtk::Widget>) {
        found.push(widget.clone());
        let mut child = widget.first_child();
        while let Some(next) = child {
            walk(&next, found);
            child = next.next_sibling();
        }
    }

    fn text_of(button: &gtk::Button) -> String {
        if let Some(label) = button.label() {
            return label.to_string();
        }
        let mut all = Vec::new();
        walk(button.upcast_ref(), &mut all);
        all.iter()
            .filter_map(|w| {
                w.downcast_ref::<gtk::Label>()
                    .map(|l| l.label().to_string())
            })
            .collect()
    }

    fn button(dialog: &adw::Dialog, text: &str) -> Option<gtk::Button> {
        let mut all = Vec::new();
        walk(dialog.upcast_ref(), &mut all);
        all.iter()
            .filter_map(|w| w.downcast_ref::<gtk::Button>())
            .find(|b| text_of(b) == text)
            .cloned()
    }

    fn setup() -> (
        DesklinkWindow,
        adw::Application,
        Rc<RefCell<Vec<Command>>>,
        Sink,
    ) {
        let resources = crate::load_resources().expect("the bundled resources");
        gio::resources_register(&resources);
        let app = adw::Application::builder()
            .application_id("derx06.desklink.com.test")
            .build();
        let window = DesklinkWindow::new(&app);
        window.present();
        pump();
        let sent: Rc<RefCell<Vec<Command>>> = Rc::default();
        let recorder = sent.clone();
        let sink: Sink = Rc::new(move |command| recorder.borrow_mut().push(command));
        (window, app, sent, sink)
    }

    fn model(required: bool) -> super::super::model::Popup {
        popup(
            &offer(),
            Version(0, 3, 0),
            true,
            required,
            Source::Package(Family::Debian),
        )
    }

    #[test]
    fn remind_me_and_escape_snooze_and_skip_does_not() {
        crate::test_display::run(|| {
            let (window, _app, sent, sink) = setup();
            // Remind me tomorrow.
            let dialog = present_popup(&window, model(false), sink.clone());
            pump();
            assert!(blocked(&window), "nothing stacks on the popup");
            button(&dialog, "Remind me tomorrow")
                .unwrap()
                .emit_clicked();
            pump();
            assert!(
                matches!(sent.borrow().as_slice(), [Command::Snooze(v)] if *v == Version(0, 4, 0)),
                "{:?}",
                sent.borrow()
            );
            // Escape (the dialog's own close) means the same.
            sent.borrow_mut().clear();
            let dialog = present_popup(&window, model(false), sink.clone());
            pump();
            dialog.close();
            pump();
            assert!(matches!(sent.borrow().as_slice(), [Command::Snooze(_)]));
            // Skip: exactly one Skip, and no snooze behind it.
            sent.borrow_mut().clear();
            let dialog = present_popup(&window, model(false), sink.clone());
            pump();
            button(&dialog, "Skip this version").unwrap().emit_clicked();
            pump();
            assert!(
                matches!(sent.borrow().as_slice(), [Command::Skip(v)] if *v == Version(0, 4, 0)),
                "{:?}",
                sent.borrow()
            );
        });
    }

    #[test]
    fn the_required_popup_has_one_button_no_cross_and_closes_silently() {
        crate::test_display::run(|| {
            let (window, _app, sent, sink) = setup();
            let dialog = present_popup(&window, model(true), sink);
            pump();
            assert!(button(&dialog, "How to update").is_some());
            assert!(button(&dialog, "Remind me tomorrow").is_none());
            assert!(button(&dialog, "Skip this version").is_none());
            let mut all = Vec::new();
            walk(dialog.upcast_ref(), &mut all);
            let bar = all
                .iter()
                .find_map(|w| w.downcast_ref::<adw::HeaderBar>())
                .unwrap();
            assert!(!bar.shows_end_title_buttons(), "no close cross");
            // Escape still leaves, for this open only: nothing is sent.
            dialog.close();
            pump();
            assert!(sent.borrow().is_empty(), "{:?}", sent.borrow());
        });
    }

    #[test]
    fn copy_puts_exactly_the_static_command_on_the_clipboard() {
        crate::test_display::run(|| {
            let (window, _app, _sent, sink) = setup();
            let dialog = present_popup(&window, model(false), sink);
            pump();
            button(&dialog, "How to update").unwrap().emit_clicked();
            pump();
            button(&dialog, "Copy command").unwrap().emit_clicked();
            pump();
            let clipboard = window.clipboard();
            let provider = clipboard.content().expect("the copy is ours");
            let value = provider.value(String::static_type()).unwrap();
            assert_eq!(
                value.get::<String>().unwrap(),
                "sudo apt update && sudo apt install --only-upgrade desklink"
            );
            assert!(button(&dialog, "Done").is_some());
        });
    }
}
