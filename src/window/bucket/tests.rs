//! Tests of the Bucket page: selection rules, drawn levels, sizes and notes,
//! names and paths, and the page on a real GTK display (Xvfb).

#[test]
fn the_buckets_one_line_names_the_phone() {
    assert_eq!(
        super::one_liner("Pixel 9"),
        "A folder that is the same on this computer and Pixel 9."
    );
}

/// Select All, Shift-click and Remove from Both only reach tiles that
/// are drawn: items past the cap are never selected.
/// Selecting: a click never opens a card or enters a folder (checked with
/// real X events in the harness too; see `shots-w6c`).
#[test]
fn a_click_opens_a_card_only_when_not_selecting() {
    assert!(super::opens_on_click(false));
    assert!(!super::opens_on_click(true));
}

#[test]
fn selection_only_reaches_what_is_drawn() {
    use super::*;
    let paths: Vec<String> = (0..MAX_TILES + 50).map(|n| format!("f{n:04}")).collect();
    let mut browse = Browse::default();
    // An old selection holds a path that will not be drawn.
    browse.selected.insert("f0001".into());
    browse.selected.insert("f0349".into());
    browse.show_level(paths.iter().map(String::as_str));
    assert_eq!((browse.order.len(), browse.hidden), (MAX_TILES, 50));
    assert_eq!(browse.order.last().map(String::as_str), Some("f0299"));
    assert!(
        !browse.selected.contains("f0349"),
        "an undrawn item is dropped"
    );
    assert!(browse.selected.contains("f0001"));
    // Select All (the real method) takes the drawn part only.
    browse.select_all();
    assert!(browse.selecting);
    assert_eq!(browse.selected.len(), MAX_TILES);
    assert!(!browse.selected.contains("f0300"));
    // A small level hides nothing.
    let mut small = Browse::default();
    small.show_level(["a", "b"].into_iter());
    small.select_all();
    assert_eq!((small.selected.len(), small.hidden), (2, 0));
}

#[test]
fn a_big_folder_says_how_much_is_not_drawn() {
    use super::*;
    assert!(more_note(MAX_TILES).is_none());
    let note = more_note(MAX_TILES + 904).unwrap();
    assert!(note.contains("300 of 1204"), "{note}");
}

#[test]
fn the_size_warns_from_8_gb_and_says_full_at_10() {
    use super::*;
    assert!(size_notice(folders::BUCKET_WARN_BYTES).is_none());
    let (headline, detail) = size_notice(folders::BUCKET_WARN_BYTES + 1).unwrap();
    assert!(headline.contains("getting big") && detail.contains("of 10 GiB"));
    let (headline, _) = size_notice(folders::BUCKET_FULL_BYTES).unwrap();
    assert!(headline.contains("full (10 GiB)"));
}

use super::*;

/// The phone asked for a Bucket: the question shows on the Overview card
/// and as a badge on the sidebar's Bucket entry until it is answered.
#[test]
fn a_bucket_the_phone_asked_for_is_a_question_until_it_is_on() {
    use crate::device_links::folders::{Offer, Pair, PairView, Phase};
    let device = "asked-bucket-phone";
    assert!(!asked(device), "nothing asked");
    folders::debug_show(
        Vec::new(),
        vec![Offer {
            device: device.into(),
            id: BUCKET_ID.into(),
            name: "DeskLink Bucket".into(),
            bucket: true,
        }],
    );
    assert!(asked(device));
    assert!(!asked("another-phone"));
    // Turned on: no question any more.
    folders::debug_show(
        vec![PairView {
            pair: Pair {
                id: BUCKET_ID.into(),
                name: "DeskLink Bucket".into(),
                root: std::env::temp_dir(),
                device: device.into(),
                include_hidden: false,
                ignore: Vec::new(),
                bucket: true,
            },
            phase: Phase::UpToDate,
            files: 0,
            last_sync: None,
            conflicts: Vec::new(),
            items: Vec::new(),
            bytes: 0,
            skipped: Vec::new(),
        }],
        Vec::new(),
    );
    assert!(!asked(device));
}

#[test]
fn the_overview_card_drops_at_the_top_whatever_folder_the_page_showed() {
    assert_eq!(drop_folder(Some(""), "Trips/Rome"), "");
    assert_eq!(drop_folder(None, "Trips/Rome"), "Trips/Rome");
    assert_eq!(drop_folder(Some("A"), "Trips"), "A");
}

#[test]
fn renaming_refuses_unsafe_and_taken_names() {
    let dir = std::env::temp_dir().join(format!("desklink-rename-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("a.txt");
    std::fs::write(&file, b"x").unwrap();
    std::fs::write(dir.join("taken.txt"), b"y").unwrap();
    for bad in [
        "", "../a.txt", "x/y", ".hidden", "a\0b", "a:b", "a\\b", "a\u{7}b", "trail.", "trail ",
    ] {
        assert!(rename_to(&file, bad).is_err(), "{bad:?}");
    }
    assert!(rename_to(&file, "taken.txt").is_err());
    assert!(
        rename_to(&file, "TAKEN.TXT").is_err(),
        "capitals alone do not make a different name"
    );
    // Its own name in other capitals is fine: it is the same file.
    rename_to(&file, "A.txt").unwrap();
    rename_to(&dir.join("A.txt"), "a.txt").unwrap();
    rename_to(&file, "b.txt").unwrap();
    assert!(dir.join("b.txt").exists() && !file.exists());
    let _ = std::fs::remove_dir_all(dir);
}

/// "Replace with <phone>'s Anyway" is on a conflict's menu only after
/// "Keep the phone's" was refused for want of a trash; the three plain
/// choices are always there. (Skipped without a display.)
#[test]
fn replace_anyway_is_offered_only_after_a_trash_refusal() {
    crate::test_display::run(|| {
        use crate::device_links::folders::{Conflict, Pair, PairView, Phase, Side};
        let view = PairView {
            pair: Pair {
                id: BUCKET_ID.into(),
                name: "DeskLink Bucket".into(),
                root: std::env::temp_dir(),
                device: "gate-phone".into(),
                include_hidden: false,
                ignore: Vec::new(),
                bucket: true,
            },
            phase: Phase::UpToDate,
            files: 1,
            last_sync: None,
            conflicts: vec![Conflict {
                path: "c.txt".into(),
                copy: "c (conflicted copy).txt".into(),
                at: 0,
            }],
            items: Vec::new(),
            bytes: 0,
            skipped: Vec::new(),
        };
        let item = Item {
            dir: false,
            path: "c.txt".into(),
            size: 1,
            mtime: 0,
            from: Side::Desk,
            state: ItemState::Conflict,
        };
        let info = crate::device_links::device_info::DeviceInfo::local(
            "gate-phone".into(),
            "Pixel".into(),
        );
        let device = DeviceView::from_info(&info, "192.168.1.9".into(), true);
        let labels = || -> Vec<String> {
            let button = item_menu(&item, Path::new("/tmp/c.txt"), &device, &view);
            let popover = button.popover().and_downcast::<gtk::Popover>().unwrap();
            let menu = popover.child().unwrap();
            let mut labels = Vec::new();
            let mut child = menu.first_child();
            while let Some(next) = child {
                if let Some(button) = next.downcast_ref::<gtk::Button>() {
                    labels.extend(button.label().map(|label| label.to_string()));
                }
                child = next.next_sibling();
            }
            labels
        };
        let before = labels();
        for choice in ["Keep both", "Keep this computer's", "Keep Pixel's"] {
            assert!(before.iter().any(|label| label == choice), "{before:?}");
        }
        assert!(
            before.iter().all(|label| !label.contains("anyway")),
            "not before a refusal: {before:?}"
        );
        folders::mark_trash_refused("gate-phone", "c.txt");
        assert!(
            labels()
                .iter()
                .any(|label| label == "Replace with Pixel's anyway"),
            "offered after the refusal"
        );
    });
}

/// The page's menus are freed with the page: a rebuilt Bucket page used
/// to leave every "more" popover alive (its items held it strongly).
/// With a display; skipped without.
#[test]
fn a_rebuilt_pages_menu_is_freed() {
    crate::test_display::run(|| {
        use crate::device_links::folders::{Pair, PairView, Phase};
        let view = PairView {
            pair: Pair {
                id: BUCKET_ID.into(),
                name: "DeskLink Bucket".into(),
                root: std::env::temp_dir(),
                device: "leak-phone".into(),
                include_hidden: false,
                ignore: Vec::new(),
                bucket: true,
            },
            phase: Phase::UpToDate,
            files: 0,
            last_sync: None,
            conflicts: Vec::new(),
            items: Vec::new(),
            bytes: 0,
            skipped: Vec::new(),
        };
        let info = crate::device_links::device_info::DeviceInfo::local(
            "leak-phone".into(),
            "Phone".into(),
        );
        let device = DeviceView::from_info(&info, "192.168.1.9".into(), true);
        let weak: Vec<glib::WeakRef<gtk::Popover>> = (0..20)
            .map(|_| {
                let button = more_menu(&view, &device);
                let popover = button.popover().and_downcast::<gtk::Popover>().unwrap();
                popover.downgrade()
            })
            .collect();
        let context = glib::MainContext::default();
        for _ in 0..20 {
            while context.iteration(false) {}
        }
        let leaked = weak
            .iter()
            .filter(|popover| popover.upgrade().is_some())
            .count();
        assert_eq!(leaked, 0, "{leaked} of 20 menus outlived their page");
    });
}
