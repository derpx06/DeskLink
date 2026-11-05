//! The DeskLink UI kit: the components of `docs/UI-REDESIGN.md` 2.8 as small
//! GTK builders, styled by `src/ui-kit.css` and the `dl_*` tokens in
//! [`tokens`]. The phone has the same set as composables.
//!
//! Nothing here changes behaviour: a builder returns a widget with the right
//! classes, accessible name and size, and the caller connects it. The
//! redesign waves (D2 to D8) use these instead of ad hoc CSS classes.
//!
//! - Colours are tokens, never hex; sizes are the scale in `ui-kit.css`.
//! - Desk hit targets are 40 px (`UI-REDESIGN.md` 2.4): [`MIN_HIT`]. A chip
//!   is a 32 px face on a 40 px hit box.
//! - Icons are `desklink-<name>-symbolic` (Material Symbols Rounded, the
//!   phone's `sym_<name>` set); [`icons`] lists them.
//! - The older classes (`round-action`, `soft-button`, ...) keep working
//!   until the last screen wave moves its users over; `.dl-card` becomes the
//!   kit's card then (today the kit's is `.dl-card.dl-card-new`).

// The waves that use the kit land after it.
#![allow(dead_code)]

pub(crate) mod banner;
pub(crate) mod icons;
mod sheet;
pub(crate) mod tokens;

pub(crate) use sheet::present_sheet;

use adw::prelude::*;
use gtk::glib;

/// The spacing scale (px, 4-pt grid).
pub(crate) const SPACING: [i32; 7] = [4, 8, 12, 16, 24, 32, 48];

/// Smallest height and width of anything clickable on the desk.
pub(crate) const MIN_HIT: i32 = 40;

/// The radii of the shape scale (px); 999 is a stadium.
pub(crate) const RADII: [i32; 7] = [3, 8, 12, 16, 24, 28, 999];

/// Which button (`UI-REDESIGN.md` 2.8.1 to 2.8.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ButtonKind {
    /// Filled accent. At most one per screen or dialog.
    Primary,
    /// Accent container fill.
    Tonal,
    /// Outlined, accent text.
    Outline,
    /// No box: accent text.
    Text,
    /// Error container fill ("Unpair…", "Delete").
    Danger,
    /// The solid red confirm button of a destructive dialog.
    DangerSolid,
}

impl ButtonKind {
    pub(crate) fn classes(self) -> &'static [&'static str] {
        match self {
            Self::Primary => &["dl-btn", "dl-btn-primary"],
            Self::Tonal => &["dl-btn", "dl-btn-tonal"],
            Self::Outline => &["dl-btn", "dl-btn-outline"],
            Self::Text => &["dl-btn", "dl-btn-text"],
            Self::Danger => &["dl-btn", "dl-btn-danger"],
            Self::DangerSolid => &["dl-btn", "dl-btn-danger", "solid"],
        }
    }
}

/// A state, for the status dot and the status pill (`UI-REDESIGN.md`
/// 2.8.12). Each has its own shape as well as its colour.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Status {
    /// Connected, done: a disc.
    Ok,
    /// Connecting, reconnecting, nothing yet: a ring.
    Idle,
    /// Can't reach, failed: a triangle.
    Err,
    /// Needs you: a diamond.
    Warn,
    /// The phone is using this computer: a square.
    Live,
}

impl Status {
    pub(crate) const ALL: [Status; 5] = [Self::Ok, Self::Idle, Self::Err, Self::Warn, Self::Live];

    pub(crate) fn class(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Idle => "idle",
            Self::Err => "err",
            Self::Warn => "warn",
            Self::Live => "live",
        }
    }
}

/// How a tile looks (`UI-REDESIGN.md` 2.8.7).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TileKind {
    Normal,
    /// The one filled tile of a screen.
    Primary,
    /// A toggle that is on.
    On,
}

impl TileKind {
    pub(crate) fn classes(self) -> &'static [&'static str] {
        match self {
            Self::Normal => &["dl-tile"],
            Self::Primary => &["dl-tile", "primary"],
            Self::On => &["dl-tile", "on"],
        }
    }
}

/// The colour of an icon square.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tone {
    Neutral,
    Accent,
    Success,
    Warn,
    Error,
    Live,
}

impl Tone {
    pub(crate) fn classes(self) -> &'static [&'static str] {
        match self {
            Self::Neutral => &["dl-icon-square"],
            Self::Accent => &["dl-icon-square", "accent"],
            Self::Success => &["dl-icon-square", "success"],
            Self::Warn => &["dl-icon-square", "warn"],
            Self::Error => &["dl-icon-square", "error"],
            Self::Live => &["dl-icon-square", "live"],
        }
    }
}

/// The classes of a pill: the base and the state.
pub(crate) fn pill_classes(status: Status) -> [&'static str; 2] {
    ["dl-pill", status.class()]
}

/// "42%": the text beside a progress bar (always determinate, always with
/// the number). Clamped to 0 to 100.
pub(crate) fn percent_text(fraction: f64) -> String {
    format!("{}%", (fraction.clamp(0.0, 1.0) * 100.0).round() as u32)
}

fn label_property(widget: &impl IsA<gtk::Accessible>, label: &str) {
    widget.update_property(&[gtk::accessible::Property::Label(label)]);
}

fn icon(name: &str, size: i32) -> gtk::Image {
    gtk::Image::builder()
        .icon_name(name)
        .pixel_size(size)
        .css_classes(["dl-glyph"])
        .accessible_role(gtk::AccessibleRole::Presentation)
        .build()
}

/// A button of one kind, with an optional 20 px icon before the label.
pub(crate) fn button(kind: ButtonKind, label: &str, icon_name: Option<&str>) -> gtk::Button {
    let button = gtk::Button::builder().css_classes(kind.classes()).build();
    match icon_name {
        None => button.set_label(label),
        Some(name) => {
            let content = gtk::Box::builder()
                .spacing(8)
                .halign(gtk::Align::Center)
                .css_classes(["dl-btn-content"])
                .build();
            content.append(&icon(name, 20));
            content.append(&gtk::Label::new(Some(label)));
            button.set_child(Some(&content));
            label_property(&button, label);
        }
    }
    button
}

/// A round, flat 40 px icon button. Icon-only, so the tooltip is also its
/// accessible name.
pub(crate) fn icon_button(icon_name: &str, tooltip: &str) -> gtk::Button {
    let button = gtk::Button::builder()
        .icon_name(icon_name)
        .tooltip_text(tooltip)
        .css_classes(["dl-icon-btn"])
        .build();
    label_property(&button, tooltip);
    button
}

/// A filter chip (a toggle). Group several with `set_group` for a single choice.
pub(crate) fn chip(label: &str, icon_name: Option<&str>, selected: bool) -> gtk::ToggleButton {
    let face = gtk::Box::builder()
        .spacing(6)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .css_classes(["dl-chip-face"])
        .build();
    if let Some(name) = icon_name {
        face.append(&icon(name, 18));
    }
    face.append(&gtk::Label::new(Some(label)));
    let chip = gtk::ToggleButton::builder()
        .css_classes(["dl-chip"])
        .child(&face)
        .active(selected)
        .build();
    label_property(&chip, label);
    chip
}

/// The status dot: a disc, ring, triangle, diamond or square by state.
/// Decoration: the word beside it carries the meaning.
pub(crate) fn status_dot(status: Status) -> gtk::Widget {
    if status == Status::Err {
        return gtk::Image::builder()
            .icon_name("desklink-warning-symbolic")
            .pixel_size(12)
            .css_classes(["dl-dot", "err"])
            .valign(gtk::Align::Center)
            .accessible_role(gtk::AccessibleRole::Presentation)
            .build()
            .upcast();
    }
    gtk::Box::builder()
        .css_classes(["dl-dot", status.class()])
        .valign(gtk::Align::Center)
        .halign(gtk::Align::Center)
        .accessible_role(gtk::AccessibleRole::Presentation)
        .build()
        .upcast()
}

/// The status pill: dot and word. It is a polite live region, so a screen
/// reader says the new word when it changes ([`update_pill`]).
pub(crate) fn status_pill(status: Status, text: &str) -> gtk::Box {
    let pill = gtk::Box::builder()
        .spacing(8)
        .halign(gtk::Align::Start)
        .valign(gtk::Align::Center)
        .css_classes(pill_classes(status))
        .accessible_role(gtk::AccessibleRole::Status)
        .build();
    pill.append(&status_dot(status));
    pill.append(&gtk::Label::new(Some(text)));
    label_property(&pill, text);
    pill
}

/// Changes a pill made by [`status_pill`].
pub(crate) fn update_pill(pill: &gtk::Box, status: Status, text: &str) {
    for known in Status::ALL {
        pill.remove_css_class(known.class());
    }
    pill.add_css_class(status.class());
    if let Some(dot) = pill.first_child() {
        pill.remove(&dot);
    }
    pill.prepend(&status_dot(status));
    if let Some(label) = pill.last_child().and_downcast::<gtk::Label>() {
        label.set_text(text);
    }
    label_property(pill, text);
}

/// A segmented control: one choice of a few, as a tab list. `on_change` gets
/// the index when the choice changes (not for the initial one).
pub(crate) fn segmented(
    labels: &[&str],
    selected: usize,
    on_change: impl Fn(usize) + 'static,
) -> gtk::Box {
    let track = gtk::Box::builder()
        .spacing(4)
        .homogeneous(true)
        .css_classes(["dl-segmented"])
        .accessible_role(gtk::AccessibleRole::TabList)
        .build();
    let on_change = std::rc::Rc::new(on_change);
    let mut first: Option<gtk::ToggleButton> = None;
    for (index, label) in labels.iter().enumerate() {
        let segment = gtk::ToggleButton::builder()
            .label(*label)
            .css_classes(["dl-segment"])
            .accessible_role(gtk::AccessibleRole::Tab)
            .build();
        match &first {
            Some(first) => segment.set_group(Some(first)),
            None => first = Some(segment.clone()),
        }
        segment.set_active(index == selected);
        segment.update_state(&[gtk::accessible::State::Selected(Some(index == selected))]);
        let on_change = on_change.clone();
        segment.connect_toggled(move |segment| {
            segment.update_state(&[gtk::accessible::State::Selected(Some(segment.is_active()))]);
            if segment.is_active() {
                on_change(index);
            }
        });
        track.append(&segment);
    }
    track
}

/// A quick-action tile: icon, title, one line of hint (the reason, when the
/// tile is insensitive).
pub(crate) fn tile(
    icon_name: &str,
    title: &str,
    hint: Option<&str>,
    kind: TileKind,
) -> gtk::Button {
    let words = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .valign(gtk::Align::Center)
        .hexpand(true)
        .build();
    words.append(
        &gtk::Label::builder()
            .label(title)
            .xalign(0.0)
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .lines(2)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["dl-tile-title"])
            .build(),
    );
    if let Some(hint) = hint {
        words.append(
            &gtk::Label::builder()
                .label(hint)
                .xalign(0.0)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .css_classes(["dl-tile-hint"])
                .build(),
        );
    }
    let content = gtk::Box::builder().spacing(12).build();
    content.append(
        &gtk::Image::builder()
            .icon_name(icon_name)
            .valign(gtk::Align::Center)
            .css_classes(["dl-tile-icon"])
            .accessible_role(gtk::AccessibleRole::Presentation)
            .build(),
    );
    content.append(&words);
    let tile = gtk::Button::builder()
        .css_classes(kind.classes())
        .child(&content)
        .build();
    label_property(&tile, title);
    if let Some(hint) = hint {
        tile.update_property(&[gtk::accessible::Property::Description(hint)]);
    }
    tile
}

/// A 40 px rounded square with a 24 px glyph, for list rows and banners.
pub(crate) fn icon_square(icon_name: &str, tone: Tone) -> gtk::CenterBox {
    gtk::CenterBox::builder()
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .css_classes(tone.classes())
        .center_widget(&icon(icon_name, 24))
        .build()
}

/// A list row: icon square, title (wraps), up to three lines of secondary
/// text. Put the trailing widget (chevron, switch, value, button) with
/// `add_suffix`.
pub(crate) fn list_row(
    icon_name: &str,
    tone: Tone,
    title: &str,
    subtitle: Option<&str>,
) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(glib::markup_escape_text(title))
        .css_classes(["dl-row"])
        .build();
    if let Some(subtitle) = subtitle {
        row.set_subtitle(&glib::markup_escape_text(subtitle));
        row.set_subtitle_lines(3);
    }
    row.add_prefix(&icon_square(icon_name, tone));
    row
}

/// Rows in one card, with hairline dividers between them.
pub(crate) fn row_group(rows: &[adw::ActionRow]) -> gtk::ListBox {
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["dl-row-group"])
        .build();
    for row in rows {
        list.append(row);
    }
    list
}

/// A card: a vertical box on `surface`, with an optional title.
pub(crate) fn card(title: Option<&str>) -> gtk::Box {
    let card = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .css_classes(["dl-card", "dl-card-new"])
        .build();
    if let Some(title) = title {
        card.append(
            &gtk::Label::builder()
                .label(title)
                .xalign(0.0)
                .wrap(true)
                .css_classes(["dl-card-title"])
                .accessible_role(gtk::AccessibleRole::Heading)
                .build(),
        );
    }
    card
}

/// A section header: 13/600 dim, a level 2 heading for screen readers.
pub(crate) fn section_header(text: &str) -> gtk::Label {
    let header = gtk::Label::builder()
        .label(text)
        .xalign(0.0)
        .wrap(true)
        .css_classes(["dl-section"])
        .accessible_role(gtk::AccessibleRole::Heading)
        .build();
    header.update_property(&[gtk::accessible::Property::Level(2)]);
    header
}

/// An `AdwToast` as the kit has it: 4 s, plain text. Say what happened in
/// the past tense, no full stop: "File sent to arch-desk".
pub(crate) fn toast(text: &str) -> adw::Toast {
    adw::Toast::builder()
        .title(text)
        .use_markup(false)
        .timeout(4)
        .build()
}

/// Gives an overlay's toasts the inverse look.
pub(crate) fn style_toasts(overlay: &adw::ToastOverlay) {
    overlay.add_css_class("dl-toasts");
}

/// The toast's look as a plain widget (the component sheet; an `AdwToast`
/// cannot be shown on its own).
pub(crate) fn toast_preview(text: &str) -> gtk::Box {
    let preview = gtk::Box::builder()
        .halign(gtk::Align::Start)
        .css_classes(["dl-toast"])
        .build();
    preview.append(&gtk::Label::new(Some(text)));
    preview
}

/// A determinate progress bar, 6 px high.
pub(crate) fn progress(fraction: f64) -> gtk::ProgressBar {
    let bar = gtk::ProgressBar::builder()
        .valign(gtk::Align::Center)
        .hexpand(true)
        .css_classes(["dl-progress"])
        .build();
    set_progress(&bar, fraction);
    bar
}

/// Sets the fraction; at 100% the bar turns success-coloured, and screen
/// readers get the percentage as the value text.
pub(crate) fn set_progress(bar: &gtk::ProgressBar, fraction: f64) {
    let fraction = fraction.clamp(0.0, 1.0);
    bar.set_fraction(fraction);
    if fraction >= 1.0 {
        bar.add_css_class("done");
    } else {
        bar.remove_css_class("done");
    }
    bar.update_property(&[gtk::accessible::Property::ValueText(&percent_text(
        fraction,
    ))]);
}

/// A bar with its `NN%` beside it, in mono with tabular figures.
pub(crate) fn progress_row(fraction: f64) -> gtk::Box {
    let row = gtk::Box::builder().spacing(12).build();
    row.append(&progress(fraction));
    row.append(
        &gtk::Label::builder()
            .label(percent_text(fraction))
            .width_chars(4)
            .xalign(1.0)
            .css_classes(["dl-progress-text"])
            .accessible_role(gtk::AccessibleRole::Presentation)
            .build(),
    );
    row
}

#[cfg(test)]
mod tests;
