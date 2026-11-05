use super::*;
use gtk::gio;

const KIT_CSS: &str = include_str!("../../ui-kit.css");
const GRESOURCE_XML: &str = include_str!("../../desklink.gresource.xml");

/// The icon names the resource file bundles as `desklink-*-symbolic`.
fn bundled_icons() -> Vec<String> {
    GRESOURCE_XML
        .lines()
        .filter_map(|line| {
            let alias = line.split("alias=\"icons/scalable/").nth(1)?;
            let file = alias.split('/').nth(1)?.split('"').next()?;
            Some(file.strip_suffix(".svg")?.to_string())
        })
        .collect()
}

/// Every `desklink-*-symbolic` literal in the sources under `dir`.
fn icon_literals(dir: &std::path::Path, found: &mut Vec<(String, String)>) {
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            icon_literals(&path, found);
        } else if path.extension().is_some_and(|ext| ext == "rs")
            // The tray writes its own icon theme to disk; its names are not bundled.
            && path.file_name().is_some_and(|name| name != "tray.rs")
        {
            let text = std::fs::read_to_string(&path).unwrap();
            let mut rest = text.as_str();
            while let Some(at) = rest.find("desklink-") {
                let word: String = rest[at..]
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
                    .collect();
                // The word must be a whole string literal, quotes on both sides.
                // (A file name such as "desklink-symbolic.svg" is not an icon name.)
                if word.ends_with("-symbolic")
                    && rest[..at].ends_with('"')
                    && rest[at + word.len()..].starts_with('"')
                {
                    found.push((word.clone(), path.display().to_string()));
                }
                rest = &rest[at + word.len().max(1)..];
            }
        }
    }
}

#[test]
fn every_design_icon_name_is_a_bundled_file() {
    let bundled = bundled_icons();
    assert!(bundled.len() > 100, "{} icons", bundled.len());
    for (design, file) in icons::DESIGN_NAMES {
        assert!(bundled.iter().any(|name| name == file), "{design}: {file}");
        assert!(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("src/icons")
                .join(format!("{file}.svg"))
                .is_file(),
            "{file}.svg"
        );
    }
    assert_eq!(
        icons::design_icon("bell"),
        Some("desklink-notifications-symbolic")
    );
    assert_eq!(icons::design_icon("nope"), None);
}

#[test]
fn every_icon_literal_in_the_sources_is_in_the_bundle() {
    let bundled = bundled_icons();
    let mut found = Vec::new();
    icon_literals(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut found,
    );
    assert!(found.len() > 10, "{found:?}");
    for (name, file) in found {
        assert!(
            bundled.contains(&name),
            "{name} (in {file}) is not in the resource bundle"
        );
    }
}

#[test]
fn every_icon_file_is_registered_and_every_entry_has_a_file() {
    let bundled = bundled_icons();
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/icons");
    for name in &bundled {
        assert!(
            dir.join(format!("{name}.svg")).is_file(),
            "{name}.svg is missing"
        );
    }
    for entry in std::fs::read_dir(&dir).unwrap().flatten() {
        let file = entry.file_name().to_string_lossy().to_string();
        if let Some(name) = file.strip_suffix(".svg") {
            assert!(
                bundled.iter().any(|b| b == name),
                "{file} is not in desklink.gresource.xml"
            );
            let svg = std::fs::read_to_string(entry.path()).unwrap();
            assert!(svg.contains("<path") && !svg.contains("<style"), "{file}");
        }
    }
}

/// The compiled bundle really holds them (what GTK looks up).
#[test]
fn the_compiled_resources_hold_every_icon() {
    let resources = crate::load_resources().expect("the bundled resources");
    for name in bundled_icons() {
        let path = format!("/derx06/desklink/com/icons/scalable/actions/{name}.svg");
        let found = resources.lookup_data(&path, gio::ResourceLookupFlags::NONE);
        let found = found.or_else(|_| {
            resources.lookup_data(
                &path.replace("/actions/", "/devices/"),
                gio::ResourceLookupFlags::NONE,
            )
        });
        assert!(found.is_ok(), "{name} is not in the compiled resources");
    }
    assert!(resources
        .lookup_data(
            "/derx06/desklink/com/ui-kit.css",
            gio::ResourceLookupFlags::NONE
        )
        .is_ok());
}

#[test]
fn buttons_tiles_pills_and_tones_map_to_their_classes() {
    assert_eq!(ButtonKind::Primary.classes(), &["dl-btn", "dl-btn-primary"]);
    assert_eq!(
        ButtonKind::DangerSolid.classes(),
        &["dl-btn", "dl-btn-danger", "solid"]
    );
    assert_eq!(TileKind::Primary.classes(), &["dl-tile", "primary"]);
    assert_eq!(TileKind::On.classes(), &["dl-tile", "on"]);
    assert_eq!(pill_classes(Status::Live), ["dl-pill", "live"]);
    assert_eq!(Tone::Warn.classes(), &["dl-icon-square", "warn"]);
    // One class per state, no two the same.
    let mut seen = std::collections::HashSet::new();
    for status in Status::ALL {
        assert!(seen.insert(status.class()));
    }
}

#[test]
fn progress_text_is_a_clamped_whole_percent() {
    assert_eq!(percent_text(0.0), "0%");
    assert_eq!(percent_text(0.424), "42%");
    assert_eq!(percent_text(0.426), "43%");
    assert_eq!(percent_text(7.0), "100%");
    assert_eq!(percent_text(-1.0), "0%");
}

fn without_comments(css: &str) -> String {
    let mut plain = String::new();
    let mut rest = css;
    while let Some(start) = rest.find("/*") {
        plain.push_str(&rest[..start]);
        rest = &rest[start + 2..];
        rest = &rest[rest.find("*/").expect("closed comment") + 2..];
    }
    plain.push_str(rest);
    plain
}

/// The stylesheet's rules, comments stripped: (selectors, declarations).
fn rules(css: &str) -> Vec<(String, String)> {
    without_comments(css)
        .split('}')
        .filter_map(|block| {
            let (selectors, body) = block.split_once('{')?;
            Some((selectors.trim().to_string(), body.trim().to_string()))
        })
        .collect()
}

fn declarations(body: &str) -> Vec<(&str, &str)> {
    body.split(';')
        .filter_map(|d| d.split_once(':'))
        .map(|(k, v)| (k.trim(), v.trim()))
        .collect()
}

#[test]
fn the_stylesheet_has_no_colour_literals_and_only_sizes_in_the_scale() {
    for (selectors, body) in rules(KIT_CSS) {
        for (property, value) in declarations(&body) {
            let what = format!("{selectors} {{ {property}: {value} }}");
            assert!(!value.contains('#'), "a hex colour: {what}");
            assert!(!value.contains("rgb"), "an rgb colour: {what}");
            for word in ["white", "black"] {
                assert!(
                    !value.split(|c: char| !c.is_alphabetic()).any(|w| w == word),
                    "{what}"
                );
            }
            if property == "font-size" {
                assert!(value.ends_with("em"), "font sizes are em: {what}");
            }
            if property == "border-radius" {
                for part in value.split_whitespace() {
                    let px: i32 = part.trim_end_matches("px").parse().unwrap();
                    assert!(
                        RADII.contains(&px) || px == 1 || px == 2 || px == 0,
                        "{what}"
                    );
                }
            }
            if property == "padding" || property == "margin" {
                for part in value.split_whitespace() {
                    let px: i32 = part.trim_end_matches("px").parse().unwrap();
                    // 20 is the buttons' side padding (UI-REDESIGN 2.8.1).
                    assert!(px == 0 || px == 20 || SPACING.contains(&px), "{what}");
                }
            }
        }
    }
}

#[test]
fn clickable_things_are_at_least_40_px() {
    let clickable = [
        ".dl-btn",
        ".dl-icon-btn",
        ".dl-chip",
        ".dl-segment",
        ".dl-tile",
        ".dl-row",
    ];
    let mut checked = Vec::new();
    for (selectors, body) in rules(KIT_CSS) {
        if !clickable.contains(&selectors.as_str()) {
            continue;
        }
        let min = declarations(&body)
            .into_iter()
            .find(|(property, _)| *property == "min-height")
            .unwrap_or_else(|| panic!("{selectors} has no min-height"))
            .1
            .trim_end_matches("px")
            .parse::<i32>()
            .unwrap();
        assert!(min >= MIN_HIT, "{selectors}: {min}");
        checked.push(selectors);
    }
    assert_eq!(checked.len(), clickable.len(), "{checked:?}");
}

#[test]
fn every_token_the_stylesheet_names_exists() {
    let mut used = std::collections::HashSet::new();
    let plain = without_comments(KIT_CSS);
    let mut rest = plain.as_str();
    while let Some(at) = rest.find("@dl_") {
        let name: String = rest[at + 1..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        used.insert(name);
        rest = &rest[at + 4..];
    }
    assert!(used.len() > 20);
    for name in used {
        assert!(
            tokens::value(&name, false, false).is_some(),
            "{name} is not a token"
        );
    }
}

#[test]
fn the_stylesheet_and_the_tokens_parse_without_errors_in_every_style() {
    crate::test_display::run(|| {
        for (dark, contrast) in [(false, false), (true, false), (false, true), (true, true)] {
            for modern in [false, true] {
                let provider = gtk::CssProvider::new();
                let errors = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
                let sink = errors.clone();
                provider.connect_parsing_error(move |_, section, error| {
                    sink.borrow_mut().push(format!("{section:?}: {error}"));
                });
                provider
                    .load_from_string(&format!("{}{KIT_CSS}", tokens::css(dark, modern, contrast)));
                assert!(
                    errors.borrow().is_empty(),
                    "dark={dark} hc={contrast}: {:?}",
                    errors.borrow()
                );
            }
        }
    });
}

fn with_kit(body: impl FnOnce() + Send + 'static) {
    crate::test_display::run(move || {
        let resources = crate::load_resources().expect("the bundled resources");
        gio::resources_register(&resources);
        body();
    });
}

#[test]
fn the_builders_carry_their_classes_names_and_sizes() {
    with_kit(|| {
        let save = button(ButtonKind::Primary, "Save", None);
        assert!(save.has_css_class("dl-btn") && save.has_css_class("dl-btn-primary"));
        assert_eq!(save.label().as_deref(), Some("Save"));

        let with_icon = button(
            ButtonKind::Tonal,
            "Send",
            Some("desklink-upload-file-symbolic"),
        );
        assert!(with_icon.child().is_some(), "icon and label in a box");

        let more = icon_button("desklink-more-vert-symbolic", "More");
        assert_eq!(more.tooltip_text().as_deref(), Some("More"));
        assert!(more.has_css_class("dl-icon-btn"));

        let chosen = chip("Notes", None, true);
        assert!(chosen.is_active() && chosen.has_css_class("dl-chip"));
        assert!(chosen
            .child()
            .is_some_and(|face| face.has_css_class("dl-chip-face")));

        let primary = tile(
            "desklink-folder-symbolic",
            "Files",
            Some("Open"),
            TileKind::Primary,
        );
        assert!(primary.has_css_class("primary"));

        let heading = section_header("Today");
        assert!(heading.has_css_class("dl-section"));
        assert_eq!(heading.accessible_role(), gtk::AccessibleRole::Heading);

        let panel = card(Some("Recent"));
        assert!(panel.has_css_class("dl-card") && panel.has_css_class("dl-card-new"));
        assert_eq!(panel.observe_children().n_items(), 1, "the title");

        let square = icon_square("desklink-folder-symbolic", Tone::Accent);
        assert!(square.has_css_class("accent"));

        let row = list_row(
            "desklink-folder-symbolic",
            Tone::Neutral,
            "A & B",
            Some("x < y"),
        );
        assert!(row.has_css_class("dl-row"));
        assert_eq!(row_group(&[row]).observe_children().n_items(), 1);

        let note = toast("Sent");
        assert_eq!(note.title().as_deref(), Some("Sent"));
        assert_eq!(note.timeout(), 4);
        assert!(!note.uses_markup());
    });
}

#[test]
fn a_pill_follows_its_state_and_a_segmented_control_its_choice() {
    with_kit(|| {
        let pill = status_pill(Status::Idle, "Connecting…");
        assert!(pill.has_css_class("idle") && pill.has_css_class("dl-pill"));
        update_pill(&pill, Status::Err, "Can't reach");
        assert!(pill.has_css_class("err") && !pill.has_css_class("idle"));
        let label = pill.last_child().and_downcast::<gtk::Label>().unwrap();
        assert_eq!(label.text(), "Can't reach");
        assert_eq!(pill.observe_children().n_items(), 2, "one dot, one word");
        assert!(pill.first_child().unwrap().has_css_class("dl-dot"));

        let chosen = std::rc::Rc::new(std::cell::Cell::new(None));
        let sink = chosen.clone();
        let track = segmented(&["A", "B", "C"], 0, move |i| sink.set(Some(i)));
        assert_eq!(chosen.get(), None, "the initial choice is not a change");
        let second = track
            .first_child()
            .and_then(|first| first.next_sibling())
            .and_downcast::<gtk::ToggleButton>()
            .unwrap();
        second.set_active(true);
        assert_eq!(chosen.get(), Some(1));
        let first = track
            .first_child()
            .and_downcast::<gtk::ToggleButton>()
            .unwrap();
        assert!(!first.is_active(), "one at a time");
    });
}

#[test]
fn progress_goes_green_at_the_end_and_the_sheet_builds() {
    with_kit(|| {
        let bar = progress(0.5);
        assert!(!bar.has_css_class("done") && (bar.fraction() - 0.5).abs() < 1e-9);
        set_progress(&bar, 2.0);
        assert!(bar.has_css_class("done") && (bar.fraction() - 1.0).abs() < 1e-9);
        let sheet = sheet::sheet();
        assert!(sheet.child().is_some());
    });
}
