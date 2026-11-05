//! The component sheet: every kit component in every state on one page.
//! Developer only (the `ui-kit` e2e verb of a debug build and the
//! screenshot harness); no screen of the app shows it.

use adw::prelude::*;

use super::{
    button, card, chip, icon_button, icon_square, list_row, progress_row, row_group,
    section_header, segmented, status_dot, status_pill, tile, toast_preview, ButtonKind, Status,
    TileKind, Tone,
};

fn row(spacing: i32) -> gtk::Box {
    gtk::Box::builder()
        .spacing(spacing)
        .valign(gtk::Align::Center)
        .build()
}

fn column(spacing: i32) -> gtk::Box {
    gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(spacing)
        .build()
}

fn captioned(caption: &str, content: &impl IsA<gtk::Widget>) -> gtk::Box {
    let cell = column(8);
    cell.append(&section_header(caption));
    cell.append(content);
    cell
}

fn buttons() -> gtk::Box {
    let all = column(16);
    for (caption, enabled) in [("Buttons", true), ("Buttons, disabled", false)] {
        let line = row(12);
        for (kind, label, glyph) in [
            (
                ButtonKind::Primary,
                "Send files",
                Some("desklink-upload-file-symbolic"),
            ),
            (ButtonKind::Tonal, "Open Bucket", None),
            (
                ButtonKind::Outline,
                "Pair another",
                Some("desklink-add-symbolic"),
            ),
            (ButtonKind::Text, "Learn more", None),
            (ButtonKind::Danger, "Unpair…", None),
            (ButtonKind::DangerSolid, "Unpair", None),
        ] {
            let button = button(kind, label, glyph);
            button.set_sensitive(enabled);
            line.append(&button);
        }
        let more = icon_button("desklink-more-vert-symbolic", "More");
        more.set_sensitive(enabled);
        line.append(&more);
        all.append(&captioned(caption, &line));
    }
    all
}

/// The keyboard focus ring, forced on: a window with no window manager
/// (the screenshot harness) never has the focus, so Tab alone shows nothing.
fn focus_ring() -> gtk::Box {
    let focused = |widget: &gtk::Widget| {
        widget.set_state_flags(
            gtk::StateFlags::FOCUSED | gtk::StateFlags::FOCUS_VISIBLE,
            false,
        );
    };
    let line = row(20);
    line.set_valign(gtk::Align::Center);
    let primary = button(ButtonKind::Primary, "Send files", None);
    let outline = button(ButtonKind::Outline, "Pair another", None);
    let text = button(ButtonKind::Text, "Learn more", None);
    let more = icon_button("desklink-more-vert-symbolic", "More");
    let tile = tile(
        "desklink-folder-symbolic",
        "Files",
        Some("Open"),
        TileKind::Normal,
    );
    let chip = chip("Notes", None, false);
    for widget in [
        primary.upcast_ref::<gtk::Widget>(),
        outline.upcast_ref(),
        text.upcast_ref(),
        more.upcast_ref(),
        tile.upcast_ref(),
        chip.upcast_ref(),
    ] {
        focused(widget);
        widget.set_valign(gtk::Align::Center);
        widget.set_halign(gtk::Align::Start);
        line.append(widget);
    }
    captioned("Keyboard focus (forced)", &line)
}

fn chips() -> gtk::Box {
    let all = column(16);
    let line = row(8);
    let first = chip("Notes", None, true);
    let second = chip("Snippets", None, false);
    let third = chip("Clipboard", Some("desklink-content-paste-symbolic"), false);
    second.set_group(Some(&first));
    third.set_group(Some(&first));
    line.append(&first);
    line.append(&second);
    line.append(&third);
    line.append(&{
        let off = chip("Disabled", None, false);
        off.set_sensitive(false);
        off
    });
    all.append(&captioned("Chips", &line));
    all.append(&captioned(
        "Segmented control",
        &segmented(&["Notes", "Snippets", "Clipboard"], 1, |_| {}),
    ));
    all
}

fn statuses() -> gtk::Box {
    let all = column(16);
    let dots = row(24);
    for (status, word) in [
        (Status::Ok, "Connected"),
        (Status::Idle, "Connecting…"),
        (Status::Err, "Can't reach"),
        (Status::Warn, "Needs your OK"),
        (Status::Live, "Live"),
    ] {
        let one = row(8);
        one.append(&status_dot(status));
        one.append(&gtk::Label::new(Some(word)));
        dots.append(&one);
    }
    all.append(&captioned("Status dots", &dots));
    let pills = row(8);
    for (status, word) in [
        (Status::Ok, "Connected"),
        (Status::Idle, "Reconnecting…"),
        (Status::Err, "Can't reach"),
        (Status::Warn, "Needs your OK"),
        (Status::Live, "Live"),
    ] {
        pills.append(&status_pill(status, word));
    }
    all.append(&captioned("Status pills", &pills));
    all
}

fn tiles() -> gtk::Box {
    let line = row(12);
    line.set_homogeneous(true);
    line.append(&tile(
        "desklink-upload-file-symbolic",
        "Send files",
        Some("To Pixel 9"),
        TileKind::Primary,
    ));
    line.append(&tile(
        "desklink-content-paste-symbolic",
        "Paste",
        Some("From the phone"),
        TileKind::Normal,
    ));
    line.append(&tile(
        "desklink-volume-up-symbolic",
        "Sound on",
        Some("Plays here"),
        TileKind::On,
    ));
    let off = tile(
        "desklink-desktop-windows-symbolic",
        "Screen",
        Some("Needs Wi-Fi"),
        TileKind::Normal,
    );
    off.set_sensitive(false);
    line.append(&off);
    captioned("Quick-action tiles", &line)
}

fn lists() -> gtk::Box {
    let all = column(16);
    let squares = row(8);
    for tone in [
        Tone::Neutral,
        Tone::Accent,
        Tone::Success,
        Tone::Warn,
        Tone::Error,
        Tone::Live,
    ] {
        squares.append(&icon_square("desklink-folder-symbolic", tone));
    }
    all.append(&captioned("Icon squares", &squares));
    let notifications = list_row(
        "desklink-notifications-symbolic",
        Tone::Accent,
        "Notifications",
        Some("Show what the phone shows, and reply from here"),
    );
    notifications.add_suffix(&gtk::Switch::builder().valign(gtk::Align::Center).build());
    let files = list_row(
        "desklink-folder-symbolic",
        Tone::Neutral,
        "Bucket",
        Some("Your shared folder"),
    );
    files.add_suffix(&gtk::Image::from_icon_name(
        "desklink-chevron-right-symbolic",
    ));
    let terminals = list_row("desklink-terminal-symbolic", Tone::Live, "Terminals", None);
    terminals.add_suffix(&gtk::Image::from_icon_name(
        "desklink-chevron-right-symbolic",
    ));
    all.append(&captioned(
        "List rows",
        &row_group(&[notifications, files, terminals]),
    ));
    all
}

fn cards() -> gtk::Box {
    let all = column(16);
    let one = card(Some("Recent files"));
    one.append(
        &gtk::Label::builder()
            .label("report.pdf · 2.4 MB")
            .xalign(0.0)
            .css_classes(["dl-mono"])
            .build(),
    );
    one.append(&progress_row(0.42));
    one.append(&progress_row(1.0));
    all.append(&captioned("Card, progress", &one));
    all.append(&captioned("Toast", &toast_preview("File sent to Pixel 9")));
    all
}

/// The whole sheet, in a scrolled window with the window colour behind it.
pub(crate) fn sheet() -> gtk::ScrolledWindow {
    let page = column(32);
    page.set_margin_top(32);
    page.set_margin_bottom(32);
    page.set_margin_start(32);
    page.set_margin_end(32);
    let title = gtk::Label::builder()
        .label("DeskLink UI kit")
        .xalign(0.0)
        .css_classes(["dl-hero"])
        .build();
    page.append(&title);
    for part in [
        buttons(),
        focus_ring(),
        chips(),
        statuses(),
        tiles(),
        lists(),
        cards(),
    ] {
        page.append(&part);
    }
    gtk::ScrolledWindow::builder()
        .child(&page)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .css_classes(["dl-sheet"])
        .build()
}

/// Shows the sheet in its own window.
pub(crate) fn present_sheet(parent: &impl IsA<gtk::Window>, width: i32, height: i32) {
    let window = adw::Window::builder()
        .title("UI kit")
        .default_width(width)
        .default_height(height)
        .transient_for(parent)
        .content(&sheet())
        .build();
    window.present();
}
