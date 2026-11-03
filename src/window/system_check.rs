//! Settings -> Advanced -> System check: what works on this computer and what
//! is missing, with the package that provides it on each distribution family.
//! The facts come from [`crate::platform`].

use adw::prelude::*;
use gtk::glib;

use crate::platform::{Check, Level};

fn icon(level: Level) -> &'static str {
    match level {
        Level::Ok => "emblem-ok-symbolic",
        Level::Limited => "dialog-warning-symbolic",
        Level::Missing => "dialog-error-symbolic",
    }
}

fn list(rows: &[Check]) -> gtk::ListBox {
    let distro = crate::platform::distro();
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();
    for row in rows {
        let subtitle = match row.install {
            Some(packages) if row.level != Level::Ok => {
                format!("{}\n\nInstall: {}", row.detail, packages.command(distro))
            }
            _ => row.detail.clone(),
        };
        let widget = adw::ActionRow::builder()
            .title(glib::markup_escape_text(row.title))
            .subtitle(glib::markup_escape_text(&subtitle))
            .subtitle_selectable(true)
            .subtitle_lines(0)
            .build();
        let status = gtk::Image::from_icon_name(icon(row.level));
        status.set_tooltip_text(Some(match row.level {
            Level::Ok => "Works",
            Level::Limited => "Works with a limit",
            Level::Missing => "Not available",
        }));
        widget.add_prefix(&status);
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
    let copy = gtk::Button::builder()
        .label("Copy Report")
        .halign(gtk::Align::End)
        .build();
    copy.add_css_class("pill");
    let intro = gtk::Label::builder()
        .label(
            "What DeskLink found on this computer. Anything marked as missing is hidden or \
             switched off where it is used, with the reason.",
        )
        .wrap(true)
        .xalign(0.0)
        .build();
    intro.add_css_class("dim-label");
    holder.append(&intro);
    let body = gtk::Box::new(gtk::Orientation::Vertical, 0);
    holder.append(&body);
    holder.append(&copy);

    let fill = {
        let (body, copy) = (body.clone(), copy.clone());
        move || -> bool {
            while let Some(child) = body.first_child() {
                body.remove(&child);
            }
            let Some(rows) = crate::platform::report() else {
                let waiting = gtk::Label::new(Some("Checking this computer…"));
                body.append(&waiting);
                copy.set_sensitive(false);
                return false;
            };
            body.append(&list(rows));
            copy.set_sensitive(true);
            true
        }
    };
    if !fill() {
        let fill = fill.clone();
        glib::timeout_add_local(std::time::Duration::from_millis(300), move || {
            if fill() {
                glib::ControlFlow::Break
            } else {
                glib::ControlFlow::Continue
            }
        });
    }
    copy.connect_clicked(|button| {
        if let Some(rows) = crate::platform::report() {
            button
                .display()
                .clipboard()
                .set_text(&crate::platform::as_text(rows, crate::platform::distro()));
            super::toast_from(button, "Report copied");
        }
    });
    let dialog = adw::Dialog::builder()
        .title("System Check")
        .content_width(560)
        .content_height(640)
        .child(&crate::application::with_header(
            &gtk::ScrolledWindow::builder()
                .hscrollbar_policy(gtk::PolicyType::Never)
                .child(&adw::Clamp::builder().child(&holder).build())
                .build(),
        ))
        .build();
    dialog.present(Some(parent));
}
