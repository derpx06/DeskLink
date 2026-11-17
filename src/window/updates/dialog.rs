//! The popup itself: an `adw::Dialog` over the window with the picture, the
//! title, the two version chips, up to three notes, where DeskLink was
//! installed from and the buttons; "How to update" swaps the lower part for
//! the guidance in place (a short cross-fade, none with reduced motion).
//!
//! The first button takes focus so Enter works. Escape, the close cross and a
//! click outside all mean "Remind me tomorrow"; "Done" after the guidance
//! does too. The Required variant has no Remind me, no Skip and no cross, and
//! never traps: Escape closes it for this open and it returns at the next.
//! All manifest text is shown as plain text (no markup, no links).
//!
//! Sibling edges: `model`, `words`, `illustration`. Outside: `ui_kit`,
//! `crate::update` (the commands it sends).

use super::model::{guide, Guide, Popup, RELEASES_PAGE};
use super::*;
use crate::update::Command;
use std::cell::Cell;
use std::rc::Rc;

/// Where the person's choices go: the update service in the app, a recorder
/// in the tests.
pub(super) type Sink = Rc<dyn Fn(Command)>;

fn label(text: &str, classes: &[&str]) -> gtk::Label {
    label_as(text, classes, gtk::AccessibleRole::Label)
}

/// A label with its own accessible role (`Alert` makes the words a live
/// region, read when the dialog or the guidance appears: the GTK 4.12
/// stand-in for an announcement).
fn label_as(text: &str, classes: &[&str], role: gtk::AccessibleRole) -> gtk::Label {
    gtk::Label::builder()
        .accessible_role(role)
        .label(text)
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .xalign(0.0)
        .use_markup(false)
        .css_classes(classes.to_vec())
        .build()
}

fn centred(text: &str, classes: &[&str]) -> gtk::Label {
    let l = label(text, classes);
    l.set_xalign(0.5);
    l.set_justify(gtk::Justification::Center);
    l
}

fn open_link(url: &str) {
    if gio::AppInfo::launch_default_for_uri(url, gio::AppLaunchContext::NONE).is_err() {
        debug_log!("[Update] could not open the release page");
    }
}

fn notes_list(model: &Popup) -> gtk::Box {
    let list = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(10)
        .build();
    for note in &model.notes {
        let row = gtk::Box::builder()
            .spacing(12)
            .accessible_role(gtk::AccessibleRole::Group)
            .build();
        row.append(&ui_kit::icon_square(note.icon, Tone::Accent));
        let words = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .valign(gtk::Align::Center)
            .hexpand(true)
            .build();
        words.append(&label(&note.title, &["heading"]));
        if let Some(detail) = &note.detail {
            words.append(&label(detail, &["dl-secondary", "dl-caption"]));
        }
        row.append(&words);
        let spoken = match &note.detail {
            Some(detail) => format!("{}. {detail}", note.title),
            None => note.title.clone(),
        };
        row.update_property(&[gtk::accessible::Property::Label(&spoken)]);
        list.append(&row);
    }
    list
}

fn chips(model: &Popup) -> gtk::Box {
    let row = gtk::Box::builder()
        .spacing(10)
        .halign(gtk::Align::Center)
        .accessible_role(gtk::AccessibleRole::Group)
        .build();
    let chip = |text: &str, selected: bool| {
        let chip = ui_kit::chip(text, None, selected);
        chip.set_focusable(false);
        chip.set_can_target(false);
        chip
    };
    row.append(&chip(&model.from, false));
    row.append(
        &gtk::Image::builder()
            .icon_name("desklink-arrow-forward-symbolic")
            .accessible_role(gtk::AccessibleRole::Presentation)
            .build(),
    );
    row.append(&chip(&model.to, true));
    row.update_property(&[gtk::accessible::Property::Label(
        &super::words::chips_spoken(&model.from, &model.to),
    )]);
    row
}

/// The guidance panel: the command to copy (or a sentence), the note about
/// the distribution, the promise, and the buttons.
fn guidance_panel(
    model: &Popup,
    plan: &Guide,
    toasts: &adw::ToastOverlay,
    on_done: impl Fn() + 'static,
) -> (gtk::Box, gtk::Widget) {
    let mut focus: Option<gtk::Widget> = None;
    let panel = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(10)
        .build();
    if let Some((command, note)) = plan.command {
        panel.append(&label_as(
            super::words::TO_UPDATE_RUN,
            &["heading"],
            gtk::AccessibleRole::Alert,
        ));
        let shown = label(command, &["monospace", "card"]);
        shown.set_selectable(true);
        shown.set_margin_top(2);
        shown.set_margin_bottom(2);
        panel.append(&shown);
        if let Some(note) = note {
            panel.append(&label(note, &["dl-secondary", "dl-caption"]));
        }
        let copy = ui_kit::button(
            ButtonKind::Tonal,
            super::words::COPY_COMMAND,
            Some("desklink-content-copy-symbolic"),
        );
        copy.set_halign(gtk::Align::Start);
        let (toasts, command) = (toasts.clone(), command.to_string());
        copy.connect_clicked(move |button| {
            button.clipboard().set_text(&command);
            toasts.add_toast(ui_kit::toast(super::words::COMMAND_COPIED));
            button.update_property(&[gtk::accessible::Property::Label(&format!(
                "{}. {}",
                super::words::COPY_COMMAND,
                super::words::COMMAND_COPIED
            ))]);
        });
        panel.append(&copy);
        focus = Some(copy.upcast());
    }
    if let Some(text) = plan.text {
        panel.append(&label_as(text, &[], gtk::AccessibleRole::Alert));
    }
    if let Some(lag) = &plan.lag {
        panel.append(&label(lag, &["dl-secondary", "dl-caption"]));
    }
    panel.append(&label(
        super::words::NEVER_BY_ITSELF,
        &["dl-secondary", "dl-caption"],
    ));
    let buttons = gtk::Box::builder().spacing(8).build();
    let page = model
        .release_url
        .clone()
        .unwrap_or_else(|| RELEASES_PAGE.to_string());
    let notes_button = ui_kit::button(
        ButtonKind::Text,
        if plan.open_page {
            super::words::OPEN_DOWNLOAD_PAGE
        } else {
            super::words::RELEASE_NOTES
        },
        None,
    );
    notes_button.connect_clicked(move |_| open_link(&page));
    buttons.append(&notes_button);
    let done = ui_kit::button(ButtonKind::Primary, super::words::DONE, None);
    done.set_hexpand(true);
    done.set_halign(gtk::Align::End);
    done.connect_clicked(move |_| on_done());
    buttons.append(&done);
    panel.append(&buttons);
    // After the swap, focus goes to what the person came for: the command's
    // Copy button, else the way out.
    (panel, focus.unwrap_or_else(|| done.upcast()))
}

/// Builds and presents the popup over `window`. Closing it any way but
/// "Skip this version" snoozes the version (not the Required one).
pub(super) fn present(window: &DesklinkWindow, model: Popup, sink: Sink) -> adw::Dialog {
    let acted = Rc::new(Cell::new(false));
    let version = model.version;
    let plan = guide(&model);

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(14)
        .margin_top(8)
        .margin_bottom(20)
        .margin_start(24)
        .margin_end(24)
        .build();
    content.append(&super::illustration::illustration(model.required));
    content.append(&centred(&model.title, &["title-2"]));
    if model.required {
        if let Some(body) = &model.body {
            // The required reason is read out when the dialog appears.
            let reason = label_as(body, &[], gtk::AccessibleRole::Alert);
            reason.set_xalign(0.5);
            reason.set_justify(gtk::Justification::Center);
            content.append(&reason);
        }
    } else {
        content.append(&chips(&model));
        content.append(&notes_list(&model));
    }
    content.append(&centred(
        &model.installed_with,
        &["dl-secondary", "dl-caption"],
    ));

    let toasts = adw::ToastOverlay::new();
    let stack = gtk::Stack::builder()
        .transition_type(
            if gtk::Settings::default().is_some_and(|s| s.is_gtk_enable_animations()) {
                gtk::StackTransitionType::Crossfade
            } else {
                gtk::StackTransitionType::None
            },
        )
        .transition_duration(150)
        .vhomogeneous(false)
        .build();

    let dialog = adw::Dialog::builder()
        .title(&model.title)
        .content_width(440)
        .follows_content_size(false)
        .can_close(true)
        .build();

    let main = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .build();
    let how = ui_kit::button(ButtonKind::Primary, super::words::HOW_TO_UPDATE, None);
    main.append(&how);
    if !model.required {
        let later_row = gtk::Box::builder().spacing(8).homogeneous(true).build();
        let later = ui_kit::button(ButtonKind::Tonal, super::words::REMIND_TOMORROW, None);
        let skip = ui_kit::button(ButtonKind::Text, super::words::SKIP_VERSION, None);
        later.connect_clicked({
            let dialog = dialog.clone();
            move |_| {
                dialog.close();
            }
        });
        skip.connect_clicked({
            let (dialog, acted, sink) = (dialog.clone(), acted.clone(), sink.clone());
            move |_| {
                acted.set(true);
                sink(Command::Skip(version));
                dialog.close();
            }
        });
        later_row.append(&later);
        later_row.append(&skip);
        main.append(&later_row);
    }
    stack.add_named(&main, Some("main"));
    let (panel, after_swap) = guidance_panel(&model, &plan, &toasts, {
        let dialog = dialog.clone();
        move || {
            dialog.close();
        }
    });
    stack.add_named(&panel, Some("how"));
    how.connect_clicked({
        let (stack, dialog) = (stack.clone(), dialog.clone());
        move |_| {
            stack.set_visible_child_name("how");
            dialog.set_focus(Some(&after_swap));
        }
    });
    content.append(&stack);

    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_height(true)
        .max_content_height((window.height() * 8 / 10).max(320))
        .child(&content)
        .build();
    let toolbar = adw::ToolbarView::new();
    let header = adw::HeaderBar::builder()
        .show_title(false)
        .show_end_title_buttons(!model.required)
        .build();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&scroller));
    toasts.set_child(Some(&toolbar));
    dialog.set_child(Some(&toasts));
    dialog.update_property(&[
        gtk::accessible::Property::Label(&model.title),
        gtk::accessible::Property::Description(&model.installed_with),
    ]);
    dialog.set_default_widget(Some(&how));
    dialog.set_focus(Some(&how));

    let required = model.required;
    dialog.connect_closed(move |_| {
        if !required && !acted.get() {
            sink(Command::Snooze(version));
        }
    });
    dialog.present(Some(window));
    dialog
}

/// The sink the app uses: the update service's channel.
pub(super) fn app_sink() -> Sink {
    Rc::new(|command| {
        if let Some(handle) = crate::update::handle() {
            handle.send(command);
        }
    })
}

/// A popup for the screenshots and tests (debug builds): `state` is
/// `available`, `required` or `guide`, `source` a package family or
/// `flatpak`, `appimage`, `manual`.
#[cfg(debug_assertions)]
pub(in crate::window) fn debug_show(window: &DesklinkWindow, state: &str, source: &str) {
    use super::model::popup as popup_model;
    use crate::update::{Component, Family, Glyph, Note, Source, Version};
    let note = |title: &str, detail: &str, glyph| Note {
        title: title.into(),
        detail: Some(detail.into()),
        glyph,
    };
    let component = Component {
        version: Version(0, 4, 0),
        released: Some("2026-12-08".into()),
        min_supported: Version(0, 3, 0),
        required_reason: None,
        rollout: 100,
        notes: vec![
            note(
                "Files has a list view",
                "Switch between grid and list.",
                Glyph::Files,
            ),
            note(
                "Faster reconnects",
                "Back online in seconds after a Wi-Fi change.",
                Glyph::Refresh,
            ),
            note(
                "Clearer Terminals",
                "A plain message when the computer is offline.",
                Glyph::Terminal,
            ),
        ],
        notes_url: None,
        release_url: Some("https://github.com/derpx06/DeskLink/releases/tag/v0.4.0".into()),
        tarball: None,
    };
    let source = match source {
        "flatpak" => Source::Flatpak,
        "appimage" => Source::AppImage,
        "manual" => Source::Manual,
        "arch" => Source::Package(Family::Arch),
        "fedora" => Source::Package(Family::Fedora),
        _ => Source::Package(Family::Debian),
    };
    let required = state == "required";
    let model = popup_model(&component, Version(0, 3, 0), true, required, source);
    let dialog = present(window, model, app_sink());
    if state == "guide" {
        if let Some(button) = named_first_button(dialog.upcast_ref()) {
            button.emit_clicked();
        }
    }
}

#[cfg(debug_assertions)]
fn named_first_button(widget: &gtk::Widget) -> Option<gtk::Button> {
    let mut stack = vec![widget.clone()];
    while let Some(w) = stack.pop() {
        if let Some(button) = w.downcast_ref::<gtk::Button>() {
            if button.label().as_deref() == Some(super::words::HOW_TO_UPDATE) {
                return Some(button.clone());
            }
        }
        let mut child = w.first_child();
        while let Some(c) = child {
            stack.push(c.clone());
            child = c.next_sibling();
        }
    }
    None
}
