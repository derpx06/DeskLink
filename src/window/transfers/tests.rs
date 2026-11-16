//! Tests of the Transfers page: card words, fractions and rates, filters,
//! day groups, failures, previews and the page on a real GTK display (Xvfb).

use super::*;

fn row(incoming: bool, done: u64, total: u64) -> TransferRow {
    TransferRow {
        id: "t".into(),
        device_id: "phone".into(),
        name: "photo.jpg".into(),
        incoming,
        done,
        total,
        preparing: false,
        paused: false,
    }
}

#[test]
fn a_row_says_which_way_the_file_goes_and_how_much_has_moved() {
    let sending = subtitle(&row(false, 1_000, 4_000), "Pixel");
    assert!(sending.starts_with("Sending to Pixel · "), "{sending}");
    assert!(sending.contains(" of "), "{sending}");
    let receiving = subtitle(&row(true, 0, 4_000), "Pixel");
    assert!(
        receiving.starts_with("Receiving from Pixel · "),
        "{receiving}"
    );
}

#[test]
fn no_rate_is_shown_for_a_row_that_is_waiting() {
    let moving = row(true, 1_000, 4_000);
    assert!(shows_rate(&moving, false));
    assert!(!shows_rate(&moving, true), "a note says why it waits");
    let mut paused = row(true, 1_000, 4_000);
    paused.paused = true;
    assert!(!shows_rate(&paused, false));
    let mut preparing = row(false, 0, 0);
    preparing.preparing = true;
    assert!(!shows_rate(&preparing, false));
}

#[test]
fn a_dropped_connection_reads_as_paused() {
    let mut paused = row(true, 1_000, 4_000);
    paused.paused = true;
    assert_eq!(
        subtitle(&paused, "Pixel"),
        "Paused — resumes when Pixel reconnects"
    );
}

/// The lane is what the row is for telling; it sits right after the
/// direction and the label wraps instead of cutting the tail with an
/// ellipsis (a 176 px card; down to a 360 px window it is the same card).
/// (Skipped without a display.)
#[test]
fn a_rows_lane_is_said_first_and_never_ellipsized() {
    crate::test_display::run(|| {
        let (daemon, _commands) = DaemonHandle::detached();
        let mut moving = row(false, 113 << 20, 300 << 20);
        moving.id = "lane-row".into();
        use crate::device_links::webrtc::transfer_board as board;
        assert!(board::begin(moving.clone()));
        board::set_lane("lane-row", Some("fast, over Wi-Fi"));
        let text = subtitle(&moving, "emulator");
        assert!(
            text.starts_with("Sending to emulator · fast, over Wi-Fi · "),
            "{text}"
        );
        let shown = new_row(&daemon, &moving);
        assert!(shown.subtitle.wraps());
        assert_eq!(shown.subtitle.ellipsize(), gtk::pango::EllipsizeMode::None);
        board::discard("lane-row");
    });
}

/// A paused row (the link dropped mid-file) still has its Cancel button:
/// one click sends one `CancelTransfer` and the button then rests.
/// (Skipped without a display.)
#[test]
fn a_paused_rows_cancel_button_asks_the_daemon_once() {
    crate::test_display::run(|| {
        use crate::device_links::daemon::DaemonCommand;
        let (daemon, commands) = DaemonHandle::detached();
        let mut paused = row(true, 1_000, 4_000);
        paused.paused = true;
        let shown = new_row(&daemon, &paused);
        fn find(widget: &gtk::Widget) -> Option<gtk::Button> {
            if let Some(button) = widget.downcast_ref::<gtk::Button>() {
                if button.tooltip_text().as_deref() == Some("Cancel this transfer") {
                    return Some(button.clone());
                }
            }
            let mut child = widget.first_child();
            while let Some(next) = child {
                if let Some(found) = find(&next) {
                    return Some(found);
                }
                child = next.next_sibling();
            }
            None
        }
        let cancel = find(shown.row.upcast_ref()).expect("a paused row can be cancelled");
        assert!(cancel.is_sensitive());
        cancel.emit_clicked();
        assert!(matches!(
            commands.try_recv(),
            Ok(DaemonCommand::CancelTransfer(device, id)) if device == "phone" && id == "t"
        ));
        assert!(!cancel.is_sensitive(), "once is enough");
        // `emit_clicked` ignores sensitivity, as a keyboard activation can.
        cancel.emit_clicked();
        assert!(commands.try_recv().is_err(), "no second request");
    });
}

#[test]
fn a_file_being_copied_has_no_size_to_measure() {
    let mut copying = row(false, 0, 0);
    copying.preparing = true;
    assert_eq!(subtitle(&copying, "Pixel"), "Preparing to send to Pixel…");
    assert_eq!(fraction(&copying), None);
}

#[test]
fn the_bar_is_the_share_done_and_never_leaves_its_range() {
    assert_eq!(fraction(&row(false, 0, 200)), Some(0.0));
    assert_eq!(fraction(&row(false, 50, 200)), Some(0.25));
    assert_eq!(fraction(&row(false, 999, 200)), Some(1.0));
    assert_eq!(
        fraction(&row(false, 5, 0)),
        None,
        "an empty file has nothing to measure"
    );
    // Over 2 GB still measures.
    assert_eq!(fraction(&row(false, 3 << 30, 6 << 30)), Some(0.5));
}

#[test]
fn a_video_length_reads_like_a_player() {
    assert_eq!(clock(Duration::from_secs(5)), "0:05");
    assert_eq!(clock(Duration::from_secs(65)), "1:05");
    assert_eq!(clock(Duration::from_secs(3725)), "1:02:05");
}

#[test]
fn speed_and_time_left_read_naturally() {
    assert_eq!(rate_text(0.0, 1_000), "", "no estimate before any progress");
    let text = rate_text(1_000_000.0, 12_000_000);
    assert!(text.ends_with("/s · 12 s left"), "{text}");
    assert!(rate_text(1_000_000.0, 600_000_000).ends_with("10 min left"));
    assert!(rate_text(1_000.0, 10_000_000).ends_with("3 h left"));
    assert!(rate_text(1_000_000.0, 0).ends_with("/s"));
}

fn logged(name: &str, incoming: bool, ts: u64) -> LoggedFile {
    LoggedFile {
        ts,
        device_id: "phone".into(),
        device_name: "Pixel".into(),
        name: name.into(),
        path: std::path::PathBuf::from("/tmp").join(name),
        size: 10,
        incoming,
    }
}

#[test]
fn the_chips_filter_by_direction_and_by_kind() {
    let photo = logged("IMG_1.JPG", true, 1);
    let report = logged("report.pdf", false, 2);
    let backup = logged("backup.tar.gz", true, 3);
    let clip = logged("clip.mp4", true, 4);
    let notes = logged("notes.txt", false, 5);
    let all = [&photo, &report, &backup, &clip, &notes];
    let shown = |filter: Filter| -> Vec<&str> {
        all.iter()
            .filter(|entry| filter.matches(entry))
            .map(|entry| entry.name.as_str())
            .collect()
    };
    assert_eq!(shown(Filter::All).len(), 5);
    assert_eq!(
        shown(Filter::Received),
        ["IMG_1.JPG", "backup.tar.gz", "clip.mp4"]
    );
    assert_eq!(shown(Filter::Sent), ["report.pdf", "notes.txt"]);
    assert_eq!(shown(Filter::Images), ["IMG_1.JPG"]);
    assert_eq!(shown(Filter::Documents), ["report.pdf", "notes.txt"]);
    assert_eq!(shown(Filter::Archives), ["backup.tar.gz"]);
    // The chips are the six, in this order, All first.
    let labels: Vec<_> = Filter::CHIPS.iter().map(|filter| filter.label()).collect();
    assert_eq!(
        labels,
        ["All", "Received", "Sent", "Images", "Documents", "Archives"]
    );
}

#[test]
fn a_file_has_a_glyph_by_its_kind() {
    assert_eq!(glyph_for("a.JPG"), "desklink-image-symbolic");
    assert_eq!(glyph_for("a.mkv"), "desklink-movie-symbolic");
    assert_eq!(glyph_for("a.pdf"), "desklink-picture-as-pdf-symbolic");
    assert_eq!(glyph_for("a.zip"), "desklink-folder-zip-symbolic");
    assert_eq!(glyph_for("a.mp3"), "desklink-music-note-symbolic");
    assert_eq!(glyph_for("a.docx"), "desklink-description-symbolic");
    assert_eq!(glyph_for("noextension"), "desklink-description-symbolic");
}

#[test]
fn files_are_grouped_by_the_day_they_came() {
    let at = |day: i32, hour: i32| {
        glib::DateTime::from_local(2026, 10, day, hour, 0, 0.0)
            .unwrap()
            .to_unix() as u64
    };
    let now = glib::DateTime::from_local(2026, 10, 20, 15, 0, 0.0).unwrap();
    // Newest first, as the log gives them.
    let groups = day_groups(
        [
            (at(20, 14), "a"),
            (at(20, 9), "b"),
            (at(19, 22), "c"),
            (at(16, 12), "d"),
            (at(1, 12), "e"),
            (at(1, 8), "f"),
        ],
        &now,
    );
    let shown: Vec<(&str, &[&str])> = groups
        .iter()
        .map(|(day, members)| (day.as_str(), members.as_slice()))
        .collect();
    assert_eq!(
        shown,
        [
            ("Today", &["a", "b"][..]),
            ("Yesterday", &["c"][..]),
            ("Friday", &["d"][..]),
            ("1 October", &["e", "f"][..]),
        ]
    );
    assert!(day_groups(Vec::<(u64, ())>::new(), &now).is_empty());
}

#[test]
fn a_row_says_what_it_is_waiting_for_in_a_word() {
    let moving = row(false, 100, 400);
    assert_eq!(row_state(&moving, false), RowState::Moving);
    assert_eq!(
        RowState::Moving.pill(),
        None,
        "a moving row has its percent"
    );
    assert_eq!(row_state(&moving, true), RowState::Waiting);
    assert_eq!(RowState::Waiting.pill(), Some((Status::Warn, "Waiting")));
    let mut paused = row(true, 100, 400);
    paused.paused = true;
    assert_eq!(row_state(&paused, true), RowState::Paused);
    assert_eq!(RowState::Paused.pill(), Some((Status::Idle, "Paused")));
    let mut copying = row(false, 0, 0);
    copying.preparing = true;
    assert_eq!(row_state(&copying, false), RowState::Preparing);
}

#[test]
fn a_failure_reads_as_a_sentence_and_an_empty_page_says_why() {
    assert_eq!(
        reason_words("pixel 9 ran out of space"),
        "Pixel 9 ran out of space."
    );
    assert_eq!(reason_words("The file changed. "), "The file changed.");
    assert_eq!(reason_words(""), "Something went wrong.");
    let (title, body) = empty_words(Filter::All, false, "Pixel 9");
    assert_eq!(title, "No files yet");
    assert!(body.contains("Pixel\u{a0}9"), "{body}");
    // A chip with nothing under it is not "no files yet".
    let (title, _) = empty_words(Filter::Archives, true, "Pixel 9");
    assert_eq!(title, "No archives");
    let (title, _) = empty_words(Filter::Archives, false, "Pixel 9");
    assert_eq!(title, "No files yet");
}

/// The lane words of CLAUDE.md stay verbatim: the producer says exactly
/// these (rewording it breaks this test), and the row carries them.
#[test]
fn the_lane_words_are_kept_as_the_board_says_them() {
    use crate::device_links::webrtc::transfer_board as board;
    use crate::device_links::webrtc::transfer_manager::words;
    let lanes = [
        ("lane-a", words::fast_lane(), "fast, over Wi-Fi"),
        ("lane-b", words::over_webrtc(), "over the secure link"),
        (
            "lane-c",
            words::through_a_relay(),
            "through the internet (slower)",
        ),
    ];
    for (id, produced, pinned) in lanes {
        assert_eq!(produced, pinned, "CLAUDE.md pins these words");
        let mut moving = row(false, 1 << 20, 8 << 20);
        moving.id = id.into();
        assert!(board::begin(moving.clone()));
        board::set_lane(id, Some(produced));
        let text = subtitle(&moving, "Pixel");
        assert!(text.contains(&format!(" · {pinned} · ")), "{text}");
        board::discard(id);
    }
}

fn failure(id: &str) -> Failure {
    Failure {
        id: id.into(),
        device_id: "phone".into(),
        name: "report.pdf".into(),
        incoming: false,
        reason: "Out of space.".into(),
        retry: Some(std::path::PathBuf::from("/tmp/report.pdf")),
    }
}

fn buttons(widget: &gtk::Widget, found: &mut Vec<gtk::Button>) {
    if let Some(button) = widget.downcast_ref::<gtk::Button>() {
        found.push(button.clone());
    }
    let mut child = widget.first_child();
    while let Some(next) = child {
        buttons(&next, found);
        child = next.next_sibling();
    }
}

/// Dismiss removes the one failure that is drawn, and Try again sends
/// the file once and takes its row away. (Skipped without a display.)
#[test]
fn dismissing_or_retrying_the_only_failure_removes_its_row() {
    crate::test_display::run(|| {
        let (daemon, commands) = DaemonHandle::detached();
        let failures = Failures::new();
        let rows = |failures: &Failures| {
            let mut count = 0;
            let mut child = failures.list.first_child();
            while let Some(next) = child {
                count += 1;
                child = next.next_sibling();
            }
            count
        };
        failures.kept.borrow_mut().push(failure("a"));
        failures.sync(&daemon, "phone");
        assert_eq!(rows(&failures), 1);
        assert!(failures.list.is_visible());
        failures.forget("a");
        failures.sync(&daemon, "phone");
        assert_eq!(rows(&failures), 0, "dismissed");
        assert!(!failures.list.is_visible());

        failures.kept.borrow_mut().push(failure("b"));
        failures.sync(&daemon, "phone");
        let mut found = Vec::new();
        buttons(failures.list.upcast_ref(), &mut found);
        let again = found
            .iter()
            .find(|button| button.tooltip_text().as_deref() == Some("Send this file again"))
            .expect("a failed send offers Try again")
            .clone();
        again.emit_clicked();
        again.emit_clicked();
        assert!(matches!(
            commands.try_recv(),
            Ok(DaemonCommand::SendFile(device, _)) if device == "phone"
        ));
        assert!(
            commands.try_recv().is_err(),
            "once, however often it is clicked"
        );
        failures.sync(&daemon, "phone");
        assert_eq!(rows(&failures), 0, "the retried row is gone");
    });
}

/// The row's buttons hold the list weakly: nothing keeps it alive.
#[test]
fn the_failure_rows_do_not_keep_their_list_alive() {
    crate::test_display::run(|| {
        let (daemon, _commands) = DaemonHandle::detached();
        let failures = Failures::new();
        failures.kept.borrow_mut().push(failure("c"));
        failures.sync(&daemon, "phone");
        let weak = Rc::downgrade(&failures);
        drop(failures);
        assert!(weak.upgrade().is_none());
    });
}
