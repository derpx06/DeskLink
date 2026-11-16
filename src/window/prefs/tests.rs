//! Tests of the settings: the rows and groups against the shared tables, search
//! words and synonyms, escaping, glide wording, and the pages on a real GTK
//! display (Xvfb).

use super::*;

/// Searching settings finds a row by its title or its words, hides the
/// rest and any group left empty, and gives every row back when the
/// search is cleared; a row hidden on purpose never comes back.
/// (Needs a display.)
#[test]
fn searching_settings_filters_rows_and_keeps_hidden_ones_hidden() {
    crate::test_display::run(|| {
        let row = |title: &str, subtitle: &str| {
            adw::ActionRow::builder()
                .title(title)
                .subtitle(subtitle)
                .build()
        };
        let (wifi, battery, secret) = (
            row("Keep awake", "While a phone is connected"),
            row("Start at login", "Keeps this computer reachable"),
            row("Hidden row", "Not for this machine"),
        );
        secret.set_visible(false);
        let first = group("Connection");
        first.add(&wifi);
        first.add(&secret);
        let second = group("Appearance");
        second.add(&battery);
        let groups = searchable(&[first.clone(), second.clone()]);

        assert!(filter_groups(&groups, "reachable"));
        assert!(!wifi.is_visible() && battery.is_visible());
        assert!(!first.is_visible(), "a group with nothing to show goes");
        assert!(second.is_visible());
        assert!(filter_groups(&groups, "connection"), "a group's own title");
        assert!(wifi.is_visible() && !secret.is_visible());
        assert!(!filter_groups(&groups, "zzz"), "nothing matches");
        assert!(filter_groups(&groups, ""));
        assert!(wifi.is_visible() && battery.is_visible());
        assert!(!secret.is_visible(), "hidden on purpose stays hidden");
        assert!(first.is_visible() && second.is_visible());
    });
}

/// Computer-wide switches on a per-phone page say they are (J18).
#[test]
fn computer_wide_rows_say_they_apply_to_every_phone() {
    for text in [
        COMPUTER_WIDE_CLIPBOARD_SUBTITLE,
        COMPUTER_WIDE_MUSIC_SUBTITLE,
    ] {
        assert!(text.contains("every paired phone"), "{text}");
    }
    assert!(COMPUTER_WIDE_CLIPBOARD_TITLE.contains("this computer"));
}

/// The groups, in the order the phone's Settings uses too.
/// Every group title survives being read as markup, and so does a name
/// with the characters markup uses (a file, a contact, a phone).
#[test]
fn titles_and_names_with_markup_characters_read_as_typed() {
    for title in GROUPS
        .iter()
        .copied()
        .chain(["a&b<c>.txt", "Pixel & <9> \"x\"", "R&D's"])
    {
        let escaped = glib::markup_escape_text(title);
        let (_, text, _) = gtk::pango::parse_markup(&escaped, '\0')
            .unwrap_or_else(|error| panic!("{title:?} did not parse: {error}"));
        assert_eq!(text.as_str(), title);
    }
    // Raw, the heading that went missing is not markup at all.
    assert!(gtk::pango::parse_markup("Privacy & security", '\0').is_err());
}

/// With a display: a row given `esc`aped text shows it as typed, in the
/// title, the subtitle and a group's description, with no markup warning.
/// (Skipped without a display.)
#[test]
fn escaped_row_text_reads_as_typed() {
    crate::test_display::run(|| {
        let name = "a&b<c>.txt";
        let row = adw::SwitchRow::builder()
            .title(esc(format!("Show {name}")))
            .subtitle(esc(format!("Tell {name}")))
            .build();
        let label = |text: &str| {
            gtk::pango::parse_markup(text, '\0')
                .map(|(_, plain, _)| plain.to_string())
                .unwrap()
        };
        assert_eq!(label(&row.title()), format!("Show {name}"));
        // libadwaita 1.5 hands a subtitle back as the shown text.
        assert_eq!(
            plain(&row.subtitle().unwrap_or_default()),
            format!("Tell {name}")
        );
        let group = adw::PreferencesGroup::builder()
            .title(esc("Privacy & security"))
            .description(esc(format!("Answer {name}'s calls")))
            .build();
        // libadwaita 1.5 hands back the shown text, later releases the
        // markup that was set: either way it reads as typed.
        assert_eq!(plain(&group.title()), "Privacy & security");
    });
}

#[test]
fn settings_has_the_ten_groups_in_the_agreed_order() {
    assert_eq!(
        GROUPS,
        [
            "Connection",
            "Notifications",
            "Clipboard",
            "Calls",
            "Sound",
            "Control",
            "Files",
            "Privacy & security",
            "Appearance",
            "Advanced",
        ]
    );
}

/// The rows of each group, in order (nested rows of an expander are not
/// listed), with markup read.
fn row_titles(group: &adw::PreferencesGroup) -> Vec<String> {
    fn walk(widget: &gtk::Widget, titles: &mut Vec<String>) {
        if let Some(row) = widget.downcast_ref::<adw::PreferencesRow>() {
            let title = row.title();
            titles.push(
                gtk::pango::parse_markup(&title, '\0')
                    .map_or_else(|_| title.to_string(), |(_, plain, _)| plain.to_string()),
            );
            return;
        }
        let mut child = widget.first_child();
        while let Some(next) = child {
            walk(&next, titles);
            child = next.next_sibling();
        }
    }
    let mut titles = Vec::new();
    walk(group.upcast_ref(), &mut titles);
    titles
}

/// The desk's Settings are the reference for the phone's: the titles of
/// every group, in order, are the table in `docs/protocol/stageD-protocol.md`
/// ("W7 - settings parity (rows)"). Changing a row here means changing
/// that table and telling the phone. (Skipped without a display.)
#[test]
fn each_settings_group_holds_the_rows_the_contract_lists() {
    crate::test_display::run(|| {
        let info = crate::device_links::device_info::DeviceInfo::local(
            "parity-phone".into(),
            "Pixel 9".into(),
        );
        let device = DeviceView::from_info(&info, "192.168.1.9".into(), true);
        let status = crate::device_links::glide::Status {
            availability: crate::device_links::glide::Availability::Unavailable("test"),
            gliding: None,
            problem: None,
            asking: false,
            can_ask_again: false,
            ready: None,
        };
        let (daemon, _commands) = DaemonHandle::detached();
        let groups = groups(&daemon, &AppSettings::default(), &device, &status, false);
        // The table is `testdata/settings_rows.json`, which the contract
        // doc mirrors; the Appearance group has two GNOME-only rows.
        let table: serde_json::Value =
            serde_json::from_str(include_str!("../../../testdata/settings_rows.json")).unwrap();
        let table: Vec<(String, Vec<String>)> = table["groups"]
            .as_array()
            .unwrap()
            .iter()
            .map(|group| {
                let names = |key: &str| -> Vec<String> {
                    group[key]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(|row| row.as_str().unwrap().replace("{phone}", "Pixel 9"))
                        .collect()
                };
                let mut rows = names("rows");
                // Rows the phone has no counterpart for are marked in the
                // table, so the phone's copy of it can leave them out.
                for only in names("deskOnlyRows") {
                    assert!(rows.contains(&only), "{only} is not a row");
                }
                if crate::shell_dbus::extension_possible() {
                    rows.extend(names("gnomeRows"));
                }
                (group["title"].as_str().unwrap().to_string(), rows)
            })
            .collect();
        assert_eq!(groups.len(), table.len());
        for (group, (title, rows)) in groups.iter().zip(&table) {
            // libadwaita 1.5 (Ubuntu 24.04) hands back the shown text,
            // later releases the markup that was set.
            let shown = plain(&group.title());
            assert_eq!(&shown, title);
            // The relay's read-only status line (compiled in only) is not
            // a setting, so the contract does not list it.
            let mut shown_rows = row_titles(group);
            shown_rows.retain(|row| row != "Away from home");
            assert_eq!(&shown_rows, rows, "rows of {title}");
        }
    });
}

#[test]
fn the_relay_reads_as_its_host() {
    assert_eq!(
        relay_host("wss://relay.desklink.app/v1"),
        "relay.desklink.app"
    );
    assert_eq!(relay_host("wss://10.0.0.2:8443?x=1"), "10.0.0.2:8443");
    assert_eq!(relay_host("relay.example"), "relay.example");
}

#[test]
fn the_glide_row_says_what_to_turn_on_until_the_phone_allows_it() {
    let says = "Pixel needs to allow “Let Desk control this phone” first.";
    assert_eq!(glide_explained("Pixel", None, false, "Desk"), says);
    assert_eq!(glide_explained("Pixel", Some(false), false, "Desk"), says);
    assert!(
        glide_explained("Pixel", Some(true), false, "Desk").starts_with("Move the pointer past")
    );
    // A locked phone: the step, not a setting to turn on.
    assert!(glide_explained("Pixel", Some(false), true, "Desk")
        .starts_with("Unlock Pixel to use Glide"));
}

/// Each of the ten groups has one place in the list, in the groups' order.
#[test]
fn every_group_has_one_place_in_the_list() {
    let indexes: Vec<usize> = PAGES.iter().map(|page| page.1).collect();
    assert_eq!(indexes, (0..GROUPS.len()).collect::<Vec<_>>());
    assert_eq!(section_label(Section::Computer, "Pixel"), "This computer");
    assert_eq!(section_label(Section::Phone, "Pixel"), "Share with Pixel");
    assert_eq!(
        section_label(Section::Privacy, "Pixel"),
        "Privacy and safety"
    );
}

fn view_for_tests() -> (View, DaemonHandle) {
    let info = crate::device_links::device_info::DeviceInfo::local(
        "parity-phone".into(),
        "Pixel 9".into(),
    );
    let device = DeviceView::from_info(&info, "192.168.1.9".into(), true);
    let status = crate::device_links::glide::Status {
        availability: crate::device_links::glide::Availability::Unavailable("test"),
        gliding: None,
        problem: None,
        asking: false,
        can_ask_again: false,
        ready: None,
    };
    let (daemon, _commands) = DaemonHandle::detached();
    let view = build_view(
        None,
        &daemon,
        &AppSettings::default(),
        &device,
        &status,
        false,
    );
    (view, daemon)
}

fn all_titles(widget: &gtk::Widget, out: &mut Vec<String>) {
    if let Some(row) = widget.downcast_ref::<adw::PreferencesRow>() {
        let title = row.title();
        out.push(
            gtk::pango::parse_markup(&title, '\0')
                .map_or_else(|_| title.to_string(), |(_, plain, _)| plain.to_string()),
        );
    }
    let mut child = widget.first_child();
    while let Some(next) = child {
        all_titles(&next, out);
        child = next.next_sibling();
    }
}

/// In either build configuration no row turns the relay or TURN on or
/// off: compiled in, one read-only status line; compiled out, nothing.
/// (Needs a display.)
#[test]
fn no_build_has_a_relay_or_turn_toggle() {
    crate::test_display::run(|| {
        assert!(relay_status_row(false).is_none());
        let row = relay_status_row(true).expect("a status line when compiled in");
        assert!(!row.is_activatable(), "it opens nothing");
        fn has_control(widget: &gtk::Widget) -> bool {
            widget.is::<gtk::Switch>()
                || widget.is::<gtk::CheckButton>()
                || widget.is::<gtk::Entry>()
                || widget.is::<gtk::Button>()
                || {
                    let mut child = widget.first_child();
                    let mut found = false;
                    while let Some(next) = child {
                        found |= has_control(&next);
                        child = next.next_sibling();
                    }
                    found
                }
        }
        assert!(!has_control(row.upcast_ref()), "no switch, field or button");
    });
}

/// The redesign moves rows into subpages and removes none: every row title
/// of the desk-ui1 page (`testdata/settings_row_titles_desk_ui1.json`,
/// written from that page, the Away from home dialog included) is in a
/// subpage now, and each subpage is linked from the list. (Needs a display.)
#[test]
fn every_original_settings_row_is_still_reachable() {
    crate::test_display::run(|| {
        // The rows of the old page. Some depend on the machine: the two
        // GNOME rows (an extension is possible) and the relay address
        // row's title (a built-in relay server).
        let table: serde_json::Value = serde_json::from_str(include_str!(
            "../../../testdata/settings_row_titles_desk_ui1.json"
        ))
        .unwrap();
        let names = |key: &str| -> Vec<String> {
            table[key]
                .as_array()
                .unwrap()
                .iter()
                .map(|title| title.as_str().unwrap().to_string())
                .collect()
        };
        let mut before = names("always");
        if crate::shell_dbus::extension_possible() {
            before.extend(names("gnome"));
        }
        // The relay's switch, address, TURN choice and status dialog are
        // gone for good: RELAY_ENABLED is the only control. ("Status" is
        // too plain a title to look for.)
        let mut relay = names("relay");
        relay.extend(names("relayCustom"));
        relay.extend(names("relayBuiltIn"));
        let (view, _daemon) = view_for_tests();
        let mut after = Vec::new();
        for page in &view.pages {
            all_titles(page.child().unwrap().upcast_ref(), &mut after);
        }
        let shown: Vec<_> = relay
            .iter()
            .filter(|title| *title != "Status" && after.contains(title))
            .collect();
        assert!(shown.is_empty(), "relay toggles shown: {shown:?}");
        if !RELAY_ENABLED {
            assert!(!after.iter().any(|title| title == "Away from home"));
            let page_lines = PAGES.map(|(_, _, _, summary)| summary).join(" ");
            assert!(!page_lines.to_lowercase().contains("away from home"));
        }
        let missing: Vec<_> = before
            .iter()
            .filter(|title| !after.contains(title))
            .collect();
        assert!(missing.is_empty(), "rows lost: {missing:?}");
        // And the list links to every subpage.
        let mut listed = Vec::new();
        let root = view.root.find_page("list").unwrap();
        all_titles(root.child().unwrap().upcast_ref(), &mut listed);
        for title in GROUPS {
            assert!(
                listed.iter().any(|row| row == title),
                "{title} is not in the list"
            );
        }
    });
}

/// Searching shows the matching rows under their group's name, from every
/// page, and clearing the search puts each group back on its page.
#[test]
fn searching_settings_reaches_every_page_and_clearing_restores_them() {
    crate::test_display::run(|| {
        fn find(widget: &gtk::Widget) -> Option<adw::PreferencesGroup> {
            if let Some(group) = widget.downcast_ref::<adw::PreferencesGroup>() {
                return Some(group.clone());
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
        let (view, _daemon) = view_for_tests();
        let groups: Vec<adw::PreferencesGroup> = view
            .pages
            .iter()
            .map(|page| find(page.child().unwrap().upcast_ref()).unwrap())
            .collect();
        let homes: Vec<_> = groups.iter().map(|group| group.parent()).collect();
        let search = SEARCH.with(|slot| slot.borrow().upgrade()).unwrap();
        // `search-changed` is sent after a pause; without a main loop, send it.
        search.set_text("glide");
        search.emit_by_name::<()>("search-changed", &[]);
        assert_ne!(groups[5].parent(), homes[5], "Control moved to the results");
        assert!(groups[5].is_visible(), "and shows its matching rows");
        assert!(!groups[7].is_visible(), "Privacy has no glide row");
        search.set_text("");
        search.emit_by_name::<()>("search-changed", &[]);
        for (group, home) in groups.iter().zip(&homes) {
            assert_eq!(&group.parent(), home);
            assert!(group.is_visible());
        }
    });
}

/// The search reads what the rows say, not their markup: an apostrophe or
/// an ampersand typed is found, and "amp" is not in "Privacy & security".
/// (Needs a display.)
#[test]
fn search_matches_the_plain_words_of_the_rows() {
    crate::test_display::run(|| {
        let privacy = group("Privacy & security");
        let send = adw::SwitchRow::builder()
            .title(esc("Send this computer's notifications"))
            .subtitle(esc("Only from the apps you choose & never back"))
            .build();
        privacy.add(&send);
        let other = group("Sound");
        let mic = adw::SwitchRow::builder().title("Microphone").build();
        other.add(&mic);
        let groups = searchable(&[privacy.clone(), other.clone()]);
        assert!(filter_groups(&groups, "computer's"));
        assert!(send.is_visible() && !mic.is_visible());
        assert!(filter_groups(&groups, "choose & never"));
        assert!(send.is_visible());
        assert!(
            filter_groups(&groups, "privacy & security"),
            "the group's title"
        );
        assert!(!filter_groups(&groups, "amp"), "markup is not text");
        assert!(!filter_groups(&groups, "&amp;"));
        assert!(filter_groups(&groups, ""));
    });
}

/// A row whose visibility is its own (the glide arrangement, shown while
/// glide is on) is not touched by a search and its clearing.
#[test]
fn the_search_leaves_a_rows_own_visibility_alone() {
    crate::test_display::run(|| {
        let controls = group("Control");
        let glide = adw::SwitchRow::builder().title("Glide onto Pixel").build();
        let arrangement = adw::PreferencesRow::builder().visible(false).build();
        arrangement.set_widget_name(OWN_VISIBILITY);
        controls.add(&glide);
        controls.add(&arrangement);
        let groups = searchable(std::slice::from_ref(&controls));
        // Glide is turned on after the page was built; then a search and a clear.
        arrangement.set_visible(true);
        assert!(filter_groups(&groups, "glide"));
        assert!(filter_groups(&groups, ""));
        assert!(arrangement.is_visible());
        arrangement.set_visible(false);
        assert!(filter_groups(&groups, "glide"));
        assert!(filter_groups(&groups, ""));
        assert!(!arrangement.is_visible());
    });
}

/// A double click on a list row pushes its subpage once: a second push
/// of the same tag is an Adwaita-CRITICAL (seen on the real desk).
/// (Needs a display.)
#[test]
fn a_second_activation_does_not_push_the_subpage_again() {
    crate::test_display::run(|| {
        let (view, _daemon) = view_for_tests();
        let list = view.root.find_page("list").unwrap();
        let mut rows = Vec::new();
        rows_in(list.upcast_ref(), &mut rows);
        let row = rows
            .iter()
            .find(|row| row.title() == GROUPS[0])
            .expect("a list row for the first group");
        let criticals = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = criticals.clone();
        let handler = glib::log_set_handler(
            Some("Adwaita"),
            glib::LogLevels::LEVEL_CRITICAL,
            false,
            false,
            move |_, _, _| {
                counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            },
        );
        row.activate();
        row.activate();
        glib::log_remove_handler(Some("Adwaita"), handler);
        assert_eq!(criticals.load(std::sync::atomic::Ordering::Relaxed), 0);
        assert_eq!(view.root.navigation_stack().n_items(), 2);
        assert!(shows(&view.root, 0));
        OPEN.with(|slot| slot.set(None));
    });
}

/// A rebuild keeps what was typed and the subpage that was open, and a
/// phone's change moved onto a switch is not sent back as a click.
/// (Needs a display.)
#[test]
fn a_rebuild_keeps_the_query_and_the_open_page_and_a_sync_sends_nothing() {
    crate::test_display::run(|| {
        let (first, _daemon) = view_for_tests();
        let search = SEARCH.with(|slot| slot.borrow().upgrade()).unwrap();
        search.set_text("clipboard");
        search.emit_by_name::<()>("search-changed", &[]);
        OPEN.with(|slot| slot.set(Some(7)));
        drop(first);
        let (second, _daemon) = view_for_tests();
        let search = SEARCH.with(|slot| slot.borrow().upgrade()).unwrap();
        assert_eq!(search.text(), "clipboard");
        assert_eq!(
            second
                .root
                .visible_page()
                .and_then(|page| page.tag())
                .as_deref(),
            Some("page-7")
        );
        search.set_text("");
        search.emit_by_name::<()>("search-changed", &[]);
        OPEN.with(|slot| slot.set(None));

        // A switch moved by `sync_live` does not run a click's handler.
        let (daemon, commands) = DaemonHandle::detached();
        let row = adw::SwitchRow::builder().build();
        let sent = std::rc::Rc::new(std::cell::Cell::new(0));
        let counter = sent.clone();
        toggled(&row, move |_| counter.set(counter.get() + 1));
        SYNCING.with(|syncing| syncing.set(true));
        row.set_active(true);
        SYNCING.with(|syncing| syncing.set(false));
        assert_eq!(sent.get(), 0);
        row.set_active(false);
        assert_eq!(sent.get(), 1);
        drop((daemon, commands));
    });
}

/// The search table and the contract list the same rows, and every row
/// has words to be found by besides its title.
#[test]
fn every_contract_row_has_search_words_and_nothing_else_does() {
    let table: serde_json::Value =
        serde_json::from_str(include_str!("../../../testdata/settings_rows.json")).unwrap();
    let mut contract = Vec::new();
    for (index, group) in table["groups"].as_array().unwrap().iter().enumerate() {
        for key in ["rows", "gnomeRows"] {
            for title in group[key].as_array().into_iter().flatten() {
                contract.push((index, title.as_str().unwrap().to_string()));
            }
        }
    }
    let rows: Vec<(usize, String)> = ROWS
        .iter()
        .map(|row| (row.group, row.title.to_string()))
        .collect();
    assert_eq!(rows, contract);
    for row in &ROWS {
        assert!(row.words.split_whitespace().count() >= 2, "{}", row.title);
    }
    assert!(fits("Glide onto {phone}", "Glide onto Pixel 9"));
    assert!(!fits("Glide onto {phone}", "Glide onto "));
    assert!(fits("Style", "Style") && !fits("Style", "Styles"));
}

/// Every row of every page says what it does under its title, and the
/// search finds each one by its title and by a word people use for it.
/// (Needs a display.)
#[test]
fn every_row_has_a_line_and_is_found_by_its_title_and_a_synonym() {
    crate::test_display::run(|| {
        let (view, _daemon) = view_for_tests();
        let mut all = Vec::new();
        for page in &view.pages {
            rows_in(page.child().unwrap().upcast_ref(), &mut all);
        }
        let mut titles = Vec::new();
        for row in &all {
            if row.widget_name() == OWN_VISIBILITY {
                continue;
            }
            let title = plain(&row.title());
            let line = row
                .downcast_ref::<adw::ActionRow>()
                .and_then(|row| row.subtitle())
                .unwrap_or_default();
            assert!(!plain(&line).trim().is_empty(), "{title} has no line");
            titles.push(title);
        }
        let search = SEARCH.with(|slot| slot.borrow().upgrade()).unwrap();
        let shows = |query: &str, template: &str| -> bool {
            search.set_text(query);
            search.emit_by_name::<()>("search-changed", &[]);
            all.iter().any(|row| {
                fits(template, &plain(&row.title()))
                    && row.is_visible()
                    && row
                        .ancestor(adw::PreferencesGroup::static_type())
                        .is_some_and(|group| group.is_visible())
            })
        };
        for row in &ROWS {
            let title = row.title.replace("{phone}", "Pixel 9");
            if !titles.contains(&title) {
                // The GNOME rows exist only where the extension can.
                assert_eq!(row.group, 8, "{title} is not on its page");
                continue;
            }
            assert!(shows(&title, row.title), "the title finds {title}");
        }
        for (query, template) in SYNONYMS {
            assert!(shows(query, template), "{query} finds {template}");
        }
        search.set_text("");
        search.emit_by_name::<()>("search-changed", &[]);
    });
}
