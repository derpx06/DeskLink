//! Settings -> Advanced -> System check: what works on this computer and what
//! is missing, with the package that provides it on each distribution family.
//! The facts come from [`crate::platform`].

use adw::prelude::*;
use gtk::glib;

use super::ui_kit::banner::{banner, BannerKind};
use super::ui_kit::{button, list_row, ButtonKind, Tone};
use crate::platform::{Check, Level};

/// How long the dialog waits for a probe before it says it could not check.
const POLL_LIMIT: std::time::Duration = std::time::Duration::from_secs(20);

/// The glyph and tone of a row: a check, a warning, an error, each a
/// different shape as well as a colour.
fn look(level: Level) -> (&'static str, Tone) {
    match level {
        Level::Ok => ("desklink-check-circle-symbolic", Tone::Success),
        Level::Limited => ("desklink-warning-symbolic", Tone::Warn),
        Level::Missing => ("desklink-error-symbolic", Tone::Error),
    }
}

fn level_words(level: Level) -> &'static str {
    match level {
        Level::Ok => "Works",
        Level::Limited => "Works with a limit",
        Level::Missing => "Not available",
    }
}

fn list(rows: &[Check]) -> gtk::ListBox {
    let distro = crate::platform::distro();
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["dl-row-group"])
        .build();
    for row in rows {
        let subtitle = match row.install {
            Some(packages) if row.level != Level::Ok => {
                format!("{}\n\nInstall: {}", row.detail, packages.command(distro))
            }
            _ => row.detail.clone(),
        };
        let (icon, tone) = look(row.level);
        let widget = list_row(icon, tone, row.title, Some(&subtitle));
        // The whole text, to read and to copy.
        widget.set_subtitle_selectable(true);
        widget.set_subtitle_lines(0);
        widget.set_tooltip_text(Some(level_words(row.level)));
        widget.update_property(&[gtk::accessible::Property::Description(level_words(
            row.level,
        ))]);
        list.append(&widget);
    }
    list
}

/// Shows the dialog. The probe runs once, in the background at start-up; if
/// it has not finished, the dialog says so and fills in when it has.
pub(crate) fn show_dialog(parent: &impl IsA<gtk::Widget>) {
    let holder = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();
    let copy = button(
        ButtonKind::Tonal,
        "Copy report",
        Some("desklink-content-copy-symbolic"),
    );
    copy.set_halign(gtk::Align::End);
    let intro = banner(
        BannerKind::Info,
        "desklink-info-symbolic",
        "What DeskLink found on this computer",
        Some(
            "Anything marked as missing is hidden or switched off where it is used, \
             with the reason.",
        ),
    );
    holder.append(&intro.root);
    let body = gtk::Box::new(gtk::Orientation::Vertical, 0);
    holder.append(&body);
    holder.append(&copy);

    // The probe runs again every time the dialog opens (a portal or PipeWire
    // may have started since); what is known already shows at once and is
    // replaced when the new answer arrives.
    let before = crate::platform::generation();
    crate::platform::refresh();
    let fill = {
        let (body, copy) = (body.clone(), copy.clone());
        move |gave_up: bool| {
            while let Some(child) = body.first_child() {
                body.remove(&child);
            }
            let Some(rows) = crate::platform::report() else {
                let words = if gave_up {
                    "DeskLink could not check this computer in time. Close this and open it again."
                } else {
                    "Checking this computer…"
                };
                // A status, so a screen reader says it when the dialog opens.
                body.append(
                    &gtk::Label::builder()
                        .label(words)
                        .wrap(true)
                        .xalign(0.0)
                        .accessible_role(gtk::AccessibleRole::Status)
                        .build(),
                );
                copy.set_sensitive(false);
                return;
            };
            body.append(&list(&rows));
            copy.set_sensitive(true);
        }
    };
    fill(false);
    let started = std::time::Instant::now();
    // Set when the dialog closes: the poll stops with it.
    let closed = std::rc::Rc::new(std::cell::Cell::new(false));
    glib::timeout_add_local(std::time::Duration::from_millis(300), {
        let fill = fill.clone();
        let closed = closed.clone();
        move || {
            if closed.get() {
                glib::ControlFlow::Break
            } else if crate::platform::generation() > before {
                fill(false);
                glib::ControlFlow::Break
            } else if started.elapsed() > POLL_LIMIT {
                // Never poll for good: say what is known, or that nothing is.
                fill(true);
                glib::ControlFlow::Break
            } else {
                glib::ControlFlow::Continue
            }
        }
    });
    copy.connect_clicked(|button| {
        if let Some(rows) = crate::platform::report() {
            button
                .display()
                .clipboard()
                .set_text(&crate::platform::as_text(&rows, crate::platform::distro()));
            super::toast_from(button, "Report copied");
        }
    });
    let dialog = adw::Dialog::builder()
        .title("System check")
        .content_width(560)
        .content_height(640)
        .child(&crate::application::with_header(
            &gtk::ScrolledWindow::builder()
                .hscrollbar_policy(gtk::PolicyType::Never)
                .child(&adw::Clamp::builder().child(&holder).build())
                .build(),
        ))
        .build();
    dialog.connect_closed(move |_| closed.set(true));
    dialog.present(Some(parent));
}
