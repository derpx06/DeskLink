//! The banner (`UI-REDESIGN.md` 2.8.11) and the empty state (2.8.15).
//!
//! A banner is a message that stays until it is dealt with: info, success,
//! warn (needs you), error, and live (the other device is using this one,
//! always with an End button). The action goes at the end; when the page is
//! narrow the window's layout pass (`apply_layout`, via the widget name
//! `responsive`) stacks it under the words.

use adw::prelude::*;

use super::{button, icon_square, ButtonKind, Tone};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BannerKind {
    /// `surface2`: something to know.
    Info,
    /// Done.
    Success,
    /// Amber: waiting on you.
    Warn,
    /// Red: failed.
    Error,
    /// Coral: a session the other device is running here. Has an End.
    Live,
}

impl BannerKind {
    pub(crate) const ALL: [BannerKind; 5] = [
        Self::Info,
        Self::Success,
        Self::Warn,
        Self::Error,
        Self::Live,
    ];

    pub(crate) fn class(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Success => "success",
            Self::Warn => "warn",
            Self::Error => "error",
            Self::Live => "live",
        }
    }

    /// Errors are announced at once; the rest wait their turn.
    pub(crate) fn role(self) -> gtk::AccessibleRole {
        match self {
            Self::Error => gtk::AccessibleRole::Alert,
            _ => gtk::AccessibleRole::Status,
        }
    }

    fn tone(self) -> Tone {
        match self {
            Self::Info => Tone::Neutral,
            Self::Success => Tone::Success,
            Self::Warn => Tone::Warn,
            Self::Error => Tone::Error,
            Self::Live => Tone::Live,
        }
    }
}

/// A banner and the parts a caller changes.
#[derive(Clone, Debug)]
pub(crate) struct Banner {
    pub(crate) root: gtk::Box,
    pub(super) title: gtk::Label,
    pub(super) detail: gtk::Label,
    pub(super) actions: gtk::Box,
}

impl Banner {
    /// Puts a button at the end (text, tonal or outline: not a second
    /// primary).
    pub(crate) fn add_action(&self, action: &impl IsA<gtk::Widget>) {
        self.actions.append(action);
    }

    pub(crate) fn set_text(&self, title: &str, detail: Option<&str>) {
        self.title.set_label(title);
        self.detail.set_label(detail.unwrap_or_default());
        self.detail
            .set_visible(detail.is_some_and(|d| !d.is_empty()));
        self.root
            .update_property(&[gtk::accessible::Property::Label(&spoken(title, detail))]);
    }
}

/// What a screen reader says for a banner: "Title. Detail".
pub(crate) fn spoken(title: &str, detail: Option<&str>) -> String {
    match detail.filter(|detail| !detail.is_empty()) {
        Some(detail) => format!("{title}. {detail}"),
        None => title.to_string(),
    }
}

fn words(text: &str, classes: &[&str]) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .xalign(0.0)
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .css_classes(classes.to_vec())
        .build()
}

/// A banner of `kind`: a 32 px icon square, a title, an optional detail, and
/// room for actions at the end ([`Banner::add_action`]).
pub(crate) fn banner(kind: BannerKind, icon: &str, title: &str, detail: Option<&str>) -> Banner {
    let root = gtk::Box::builder()
        .spacing(12)
        .css_classes(["dl-banner", kind.class()])
        .accessible_role(kind.role())
        .build();
    root.set_widget_name("responsive");
    let lead = gtk::Box::builder().spacing(12).hexpand(true).build();
    let square = icon_square(icon, kind.tone());
    square.add_css_class("dl-banner-icon");
    square.set_valign(gtk::Align::Start);
    if let Some(glyph) = square.center_widget().and_downcast::<gtk::Image>() {
        glyph.set_pixel_size(20);
    }
    lead.append(&square);
    let text = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .hexpand(true)
        .valign(gtk::Align::Center)
        .build();
    let title_label = words(title, &["dl-banner-title"]);
    let detail_label = words(detail.unwrap_or_default(), &["dl-banner-detail"]);
    detail_label.set_visible(detail.is_some_and(|d| !d.is_empty()));
    text.append(&title_label);
    text.append(&detail_label);
    lead.append(&text);
    root.append(&lead);
    let actions = gtk::Box::builder()
        .spacing(8)
        .halign(gtk::Align::End)
        .valign(gtk::Align::Center)
        .build();
    root.append(&actions);
    let banner = Banner {
        root,
        title: title_label,
        detail: detail_label,
        actions,
    };
    banner.set_text(title, detail);
    banner
}

/// A live banner: coral, and always with its End button (`end_label` is the
/// word, "End" or "End all"); `on_end` runs when it is pressed.
pub(crate) fn live_banner(
    icon: &str,
    title: &str,
    detail: Option<&str>,
    end_label: &str,
    on_end: impl Fn(&gtk::Button) + 'static,
) -> Banner {
    let banner = banner(BannerKind::Live, icon, title, detail);
    let end = button(ButtonKind::Danger, end_label, None);
    end.connect_clicked(move |end| on_end(end));
    banner.add_action(&end);
    banner
}

/// The empty state: a 28 glyph in a 72 circle, a title, up to three lines of
/// what the place is for. Append at most one filled button to it.
pub(crate) fn empty_state(icon: &str, title: &str, body: &str) -> gtk::Box {
    let state = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .css_classes(["dl-empty"])
        .build();
    let circle = gtk::CenterBox::builder()
        .halign(gtk::Align::Center)
        .css_classes(["dl-empty-circle"])
        .center_widget(
            &gtk::Image::builder()
                .icon_name(icon)
                .pixel_size(28)
                .accessible_role(gtk::AccessibleRole::Presentation)
                .build(),
        )
        .build();
    state.append(&circle);
    state.append(
        &gtk::Label::builder()
            .label(title)
            .wrap(true)
            .justify(gtk::Justification::Center)
            .css_classes(["dl-empty-title"])
            .accessible_role(gtk::AccessibleRole::Heading)
            .build(),
    );
    state.append(
        &gtk::Label::builder()
            .label(body)
            .wrap(true)
            .max_width_chars(36)
            .justify(gtk::Justification::Center)
            .css_classes(["dl-empty-body"])
            .build(),
    );
    state
}
