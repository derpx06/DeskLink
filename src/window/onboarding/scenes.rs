//! The first run's pages and drawn scenes: the glance page, word boxes, and
//! the little laptop/phone/terminal scenes of the tour.
//!
//! Sibling edges: `steps` (the tour's words).

use super::*;

/// The glance: what DeskLink does in three lines on the left, the three
/// steps to start in a card on the right (one column when narrow).
pub(super) fn glance_page(narrow: &adw::Breakpoint) -> gtk::Box {
    let words = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .valign(gtk::Align::Center)
        .hexpand(true)
        .build();
    words.append(&label(GLANCE_KICKER, &["ob-kicker"]));
    let heading = label(GLANCE_TITLE, &["ob-title"]);
    heading.set_accessible_role(gtk::AccessibleRole::Heading);
    heading.update_property(&[gtk::accessible::Property::Level(1)]);
    words.append(&heading);
    let lines = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(14)
        .margin_top(12)
        .build();
    for (icon, line) in GLANCE_LINES {
        let row = gtk::Box::builder().spacing(14).build();
        let square = icon_square(icon, Tone::Accent);
        square.set_valign(gtk::Align::Center);
        row.append(&square);
        let words = label(line, &["ob-body", "ob-glance-line"]);
        words.set_valign(gtk::Align::Center);
        words.set_hexpand(true);
        row.append(&words);
        lines.append(&row);
    }
    words.append(&lines);

    let steps = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(16)
        .valign(gtk::Align::Center)
        .css_classes(["ob-scene-card", "ob-steps"])
        .build();
    let title = label(GLANCE_STEPS_TITLE, &["ob-steps-title"]);
    title.set_accessible_role(gtk::AccessibleRole::Heading);
    title.update_property(&[gtk::accessible::Property::Level(2)]);
    steps.append(&title);
    let list = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(14)
        .accessible_role(gtk::AccessibleRole::List)
        .build();
    for (index, step) in GLANCE_STEPS.iter().enumerate() {
        let row = gtk::Box::builder()
            .spacing(14)
            .accessible_role(gtk::AccessibleRole::ListItem)
            .build();
        let number = gtk::Label::builder()
            .label((index + 1).to_string())
            .valign(gtk::Align::Center)
            .css_classes(["ob-step-num"])
            .accessible_role(gtk::AccessibleRole::Presentation)
            .build();
        row.append(&number);
        let words = label(step, &["ob-step"]);
        words.set_valign(gtk::Align::Center);
        words.set_hexpand(true);
        row.append(&words);
        row.update_property(&[gtk::accessible::Property::Label(&format!(
            "Step {}: {step}",
            index + 1
        ))]);
        list.append(&row);
    }
    steps.append(&list);
    let footer = label(GLANCE_FOOTER, &["ob-body", "ob-glance-footer"]);
    footer.set_margin_top(4);
    steps.append(&footer);

    let layout = gtk::Box::builder()
        .spacing(40)
        .homogeneous(true)
        .valign(gtk::Align::Center)
        .vexpand(true)
        .build();
    layout.append(&words);
    layout.append(&steps);
    narrow.add_setter(
        &layout,
        "orientation",
        Some(&gtk::Orientation::Vertical.to_value()),
    );
    narrow.add_setter(&layout, "spacing", Some(&24.to_value()));
    narrow.add_setter(&layout, "homogeneous", Some(&false.to_value()));
    layout
}

/// The kicker, the title (a level 1 heading) and the body of a page.
pub(super) fn words_box(kicker: &str, title: &str, body: &str) -> gtk::Box {
    let words = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .build();
    words.append(&label(kicker, &["ob-kicker"]));
    let heading = label(title, &["ob-title"]);
    heading.set_accessible_role(gtk::AccessibleRole::Heading);
    heading.update_property(&[gtk::accessible::Property::Level(1)]);
    words.append(&heading);
    let body = label(body, &["ob-body"]);
    body.set_max_width_chars(46);
    words.append(&body);
    words
}

// ---- The scenes ----------------------------------------------------------------

/// The scenes' canvas, in px: decoration, so it keeps its size at large text.
const SCENE_W: i32 = 360;
const SCENE_H: i32 = 240;

fn shape(classes: &[&str], width: i32, height: i32) -> gtk::Box {
    let shape = gtk::Box::builder().css_classes(classes).build();
    shape.set_size_request(width, height);
    shape
}

fn glyph(name: &str, size: i32, classes: &[&str]) -> gtk::Image {
    gtk::Image::builder()
        .icon_name(name)
        .pixel_size(size)
        .css_classes(classes)
        .build()
}

fn chip(icon: &str, words: &str, delay: &str) -> gtk::Box {
    let chip = gtk::Box::builder()
        .spacing(6)
        .css_classes(["ob-chip", "float", delay])
        .build();
    chip.append(&glyph(icon, 16, &["ob-accent"]));
    chip.append(&gtk::Label::new(Some(words)));
    chip
}

/// A laptop at (x, y) whose screen is `w` x `h`; returns the screen's inside.
fn laptop(canvas: &gtk::Fixed, x: f64, y: f64, w: i32, h: i32) -> gtk::Box {
    let screen = shape(&["ob-laptop"], w, h);
    let display = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .hexpand(true)
        .css_classes(["ob-display"])
        .build();
    screen.append(&display);
    canvas.put(&screen, x, y);
    canvas.put(
        &shape(&["ob-laptop-base"], w + 28, 9),
        x - 14.0,
        y + f64::from(h),
    );
    display
}

/// A phone at (x, y), `w` x `h`; returns its screen.
fn phone(canvas: &gtk::Fixed, x: f64, y: f64, w: i32, h: i32) -> gtk::Box {
    let body = shape(&["ob-phone"], w, h);
    let display = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .hexpand(true)
        .spacing(5)
        .css_classes(["ob-phone-display"])
        .build();
    body.append(&display);
    canvas.put(&body, x, y);
    display
}

fn term_lines(lines: &[&str], classes: &[&str]) -> gtk::Box {
    let term = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .vexpand(true)
        .css_classes(classes)
        .build();
    for (index, line) in lines.iter().enumerate() {
        let delay = ["l1", "l2", "l3", "l4"][index.min(3)];
        let prompt = line.starts_with('$');
        term.append(
            &gtk::Label::builder()
                .label(*line)
                .xalign(0.0)
                .css_classes(if prompt {
                    vec!["appear", delay, "prompt"]
                } else {
                    vec!["appear", delay]
                })
                .build(),
        );
    }
    term
}

/// The scene of tour page `page` (screens, share, terminals); `still` when
/// animations are off.
pub(in crate::window) fn scene(page: usize, still: bool) -> gtk::Fixed {
    let canvas = gtk::Fixed::builder()
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .css_classes(["ob-scene"])
        .can_target(false)
        .build();
    canvas.set_size_request(SCENE_W, SCENE_H);
    if still {
        canvas.add_css_class("still");
    }
    // A picture of what the words say: not read out.
    canvas.update_state(&[gtk::accessible::State::Hidden(true)]);
    match page {
        0 => {
            let screen = laptop(&canvas, 20.0, 58.0, 196, 120);
            let window = shape(&["ob-window"], 96, 56);
            window.set_halign(gtk::Align::Start);
            window.set_margin_start(14);
            window.set_margin_top(14);
            screen.append(&window);
            canvas.put(
                &glyph("desklink-touch-app-symbolic", 22, &["ob-pointer", "drift"]),
                128.0,
                118.0,
            );
            let display = phone(&canvas, 262.0, 36.0, 82, 156);
            let mirror = shape(&["ob-mirror"], 60, 36);
            mirror.append(&shape(&["ob-window", "small"], 30, 18));
            display.append(&mirror);
            let pad = gtk::CenterBox::builder()
                .vexpand(true)
                .css_classes(["ob-touchpad"])
                .center_widget(&glyph("desklink-touchpad-mouse-symbolic", 20, &["ob-dim"]))
                .build();
            display.append(&pad);
            let keys = gtk::Box::builder().spacing(4).homogeneous(true).build();
            for _ in 0..3 {
                keys.append(&shape(&["ob-key"], 14, 10));
            }
            display.append(&keys);
            canvas.put(
                &chip("desklink-desktop-windows-symbolic", "Desk screen", "d1"),
                20.0,
                14.0,
            );
            canvas.put(
                &chip("desklink-keyboard-symbolic", "Touchpad and keyboard", "d2"),
                150.0,
                204.0,
            );
        }
        1 => {
            let screen = laptop(&canvas, 20.0, 60.0, 190, 118);
            let drop = gtk::CenterBox::builder()
                .vexpand(true)
                .css_classes(["ob-drop"])
                .center_widget(&glyph("desklink-download-symbolic", 26, &["ob-accent"]))
                .build();
            screen.append(&drop);
            let display = phone(&canvas, 264.0, 40.0, 78, 150);
            let note = gtk::Box::builder()
                .orientation(gtk::Orientation::Vertical)
                .spacing(4)
                .css_classes(["ob-note"])
                .build();
            for width in [44, 36, 40] {
                let line = shape(&["ob-line"], width, 4);
                line.set_halign(gtk::Align::Start);
                note.append(&line);
            }
            display.append(&note);
            let photo = gtk::CenterBox::builder()
                .vexpand(true)
                .css_classes(["ob-photo"])
                .center_widget(&glyph("desklink-image-symbolic", 20, &["ob-dim"]))
                .build();
            display.append(&photo);
            for (index, (icon, delay)) in [
                ("desklink-description-symbolic", "d1"),
                ("desklink-image-symbolic", "d2"),
                ("desklink-picture-as-pdf-symbolic", "d3"),
            ]
            .iter()
            .enumerate()
            {
                let file = gtk::CenterBox::builder()
                    .css_classes(["ob-file", "send", delay])
                    .center_widget(&glyph(icon, 16, &["ob-accent"]))
                    .build();
                file.set_size_request(26, 30);
                canvas.put(&file, 222.0, 74.0 + 34.0 * index as f64);
            }
            canvas.put(&chip("desklink-folder-symbolic", "Files", "d1"), 20.0, 14.0);
            canvas.put(
                &chip("desklink-content-paste-symbolic", "Clipboard", "d2"),
                112.0,
                14.0,
            );
            canvas.put(
                &chip("desklink-edit-note-symbolic", "Notes", "d3"),
                196.0,
                204.0,
            );
        }
        _ => {
            let screen = laptop(&canvas, 18.0, 50.0, 214, 130);
            screen.add_css_class("dark");
            screen.append(&term_lines(
                &[
                    "$ ssh build-box",
                    "$ cargo build --release",
                    "   Compiling desklink",
                    "    Finished in 41 s",
                ],
                &["ob-term"],
            ));
            let display = phone(&canvas, 266.0, 38.0, 78, 152);
            display.add_css_class("dark");
            display.append(&term_lines(
                &["$ ssh build", "$ cargo build", " Compiling", " Finished"],
                &["ob-term", "small"],
            ));
            canvas.put(
                &chip(
                    "desklink-terminal-symbolic",
                    "Same terminal, both screens",
                    "d1",
                ),
                76.0,
                204.0,
            );
        }
    }
    canvas
}
