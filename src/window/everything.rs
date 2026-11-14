//! Home's last section, "Everything DeskLink does": every feature this build
//! offers for the selected phone, in a few plain groups, each with what it
//! does, whether it is ready, and the one button that opens it or takes the
//! step it needs. Nothing is greyed out: what cannot run says why.
//!
//! [`rows`] is pure (the facts come in a [`Facts`]). The steps are
//! `requirements`' needs and blockers (the same as the Finish setting up
//! card, the tiles and Find anything say), and the buttons are the guide's
//! and the palette's ([`super::guide::run`]), so the four cannot disagree.
//! Every Get started card and every Find anything action has a row here
//! (tests).

use adw::prelude::*;
use std::sync::OnceLock;

use super::guide::{self, Do};
use super::palette_model::{feature_of, Action, Target};
use super::requirements::{self, blocker, Feature, Fix, Need, PhoneFacts, Reach};
use super::ui_kit::{self, ButtonKind, Status, Tone};
use super::DesklinkWindow;
use crate::device_links::daemon::{DaemonCommand, DaemonHandle};
use crate::device_links::device::DeviceView;
use crate::device_links::glide::Availability;
use crate::device_links::packet::{
    PACKET_TYPE_CLIPBOARD, PACKET_TYPE_CONTACTS_REQUEST_ALL_UIDS_TIMESTAMPS,
    PACKET_TYPE_FINDMYPHONE_REQUEST, PACKET_TYPE_FOLDERS, PACKET_TYPE_LOCK_REQUEST,
    PACKET_TYPE_MOUSEPAD_REQUEST, PACKET_TYPE_MPRIS_REQUEST, PACKET_TYPE_NOTIFICATION,
    PACKET_TYPE_PING, PACKET_TYPE_SHARE_INPUT_DEVICES_REQUEST, PACKET_TYPE_SHARE_REQUEST,
    PACKET_TYPE_TELEPHONY_REQUEST,
};
use crate::platform::Level;
use crate::shell_dbus::ExtensionStatus;

pub(super) const TITLE: &str = "Everything DeskLink does";
pub(super) const SUBTITLE: &str = "Where each thing is, and the one step it needs, if any.";

/// The groups, in the order they are shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Group {
    Share,
    Screen,
    Phone,
    Notes,
    Computer,
    Help,
}

impl Group {
    pub(super) const ALL: [Group; 6] = [
        Group::Share,
        Group::Screen,
        Group::Phone,
        Group::Notes,
        Group::Computer,
        Group::Help,
    ];

    pub(super) fn title(self) -> &'static str {
        match self {
            Self::Share => "Share and files",
            Self::Screen => "Screen and control",
            Self::Phone => "Phone and calls",
            Self::Notes => "Notes and clipboard",
            Self::Computer => "This computer",
            Self::Help => "Help",
        }
    }
}

/// Whether a feature can be used now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum State {
    Ready,
    /// One step first, in words; the button takes it when it can.
    Step(String),
    /// Not on this phone or computer, and why. No button.
    Off(String),
    /// Done on the phone; the line says where.
    OnPhone,
}

impl State {
    /// The pill: its look and its word.
    fn pill(&self) -> (Status, &'static str) {
        match self {
            Self::Ready => (Status::Ok, "Ready"),
            Self::Step(_) => (Status::Warn, "One step"),
            Self::Off(_) => (Status::Idle, "Not available"),
            Self::OnPhone => (Status::Idle, "On the phone"),
        }
    }

    /// Stable words for the Home page's key.
    fn key(&self) -> &str {
        match self {
            Self::Ready => "ready",
            Self::Step(words) | Self::Off(words) => words,
            Self::OnPhone => "phone",
        }
    }
}

/// What a row's button does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Go {
    /// What a guide card's button does (a palette target, a fix, ...).
    Do(Do),
    /// The Insert tile's menu.
    Insert,
    /// The Sound tile's menu.
    Sound,
    /// Play or pause the phone's player, if one is known.
    Music(Option<String>),
}

/// One feature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Row {
    /// The guide card's id where the guide has the feature.
    pub id: &'static str,
    pub group: Group,
    pub icon: &'static str,
    pub title: String,
    /// What it does, in a line.
    pub line: String,
    pub state: State,
    pub button: Option<(String, Go)>,
}

/// Everything [`rows`] reads.
#[derive(Debug, Clone)]
pub(super) struct Facts {
    /// The needs' facts: the phone, this computer's checks, the top bar.
    pub base: requirements::Facts,
    /// "Phone", "Tablet", ...
    pub kind: String,
    pub glide: Availability,
    /// Why Glide is not working, when it is not.
    pub glide_problem: Option<String>,
    pub clipboard_sync: bool,
    pub photos: bool,
    pub forward: bool,
    pub walk_away: bool,
    pub bucket: bool,
    /// The phone's music player, while it has one.
    pub player: Option<String>,
    /// The desktop's display settings can be opened.
    pub displays: bool,
}

impl Facts {
    /// `device` and this computer, as they are now (`base` is Home's).
    pub(super) fn now(base: requirements::Facts, device: &DeviceView) -> Self {
        static DISPLAYS: OnceLock<bool> = OnceLock::new();
        let settings = crate::device_links::config::app_settings();
        let glide = crate::device_links::glide::status();
        Self {
            base,
            kind: super::device_type_label(&device.device_type),
            glide: glide.availability,
            glide_problem: glide.problem,
            clipboard_sync: settings.clipboard_sync,
            photos: settings.gallery_devices.contains(&device.id),
            forward: settings.forward_notifications,
            walk_away: settings.walk_away_devices.contains(&device.id),
            bucket: crate::device_links::folders::bucket_on(&device.id),
            player: device
                .media_status
                .as_ref()
                .map(|media| media.player.clone())
                .filter(|player| !player.is_empty()),
            displays: *DISPLAYS
                .get_or_init(|| gtk::glib::find_program_in_path("gnome-control-center").is_some()),
        }
    }
}

/// A state and the button that goes with it.
type Gate = (State, Option<(String, Go)>);

fn ready(words: &str, go: Go) -> Gate {
    (State::Ready, Some((words.to_string(), go)))
}

fn open(words: &str, target: Target) -> Gate {
    ready(words, Go::Do(Do::Run(target)))
}

/// Off by choice: the step is to turn it on, in its Settings subpage.
fn turned_off(page: &str) -> Gate {
    (
        State::Step("It is off".into()),
        Some((
            "Turn on in Settings".into(),
            Go::Do(Do::Run(guide::settings(page))),
        )),
    )
}

/// A need as a state, its fix as the button. With nothing to click, the
/// words say what to do. What this phone's DeskLink cannot do, or what waits
/// for the phone to connect, is not a step: it says why.
fn from_need(need: &Need) -> Gate {
    let button = need
        .fix
        .label()
        .map(|label| (label.to_string(), Go::Do(Do::Fix(need.fix.clone()))));
    let words = if button.is_some() {
        need.title.clone()
    } else {
        let gap = if need.title.ends_with(['…', '.']) {
            " "
        } else {
            ". "
        };
        format!("{}{gap}{}", need.title, need.detail)
    };
    if need.id == "phone-update" || (need.id == "reach" && button.is_none()) {
        return (State::Off(words), None);
    }
    (State::Step(words), button)
}

/// Whether this build of DeskLink sends `packet` (has the feature at all).
fn sends(packet: &str) -> bool {
    crate::device_links::features::FeatureRegistry::desktop()
        .outgoing_capabilities()
        .iter()
        .any(|known| known == packet)
}

/// Every feature, group by group.
pub(super) fn rows(facts: &Facts) -> Vec<Row> {
    let Some(phone) = facts.base.phone.as_ref() else {
        return Vec::new();
    };
    let needs = requirements::missing(&facts.base);
    let need = |id: &str| needs.iter().find(|need| need.id == id).map(from_need);
    let check = |id: &str| facts.base.checks.iter().find(|check| check.id == id);
    // A check that failed: why, and its install command as the button.
    let broken = |id: &str| -> Option<Gate> {
        let check = check(id).filter(|check| check.level != Level::Ok)?;
        Some(match facts.base.install(check) {
            Some(command) => (
                State::Step(check.detail.clone()),
                Some(("Copy command".into(), Go::Do(Do::Fix(Fix::Copy(command))))),
            ),
            None => (State::Off(check.detail.clone()), None),
        })
    };
    let gate = |feature: Feature| blocker(feature, phone).map(|need| from_need(&need));
    let on_phone = |action: Action, words: &str| -> Gate {
        feature_of(action).and_then(gate).unwrap_or_else(|| {
            ready(
                words,
                Go::Do(Do::Run(Target::PhoneAction(phone.id.clone(), action))),
            )
        })
    };
    let name = phone.name.as_str();
    let kind = facts.kind.to_lowercase();
    let mut out = Vec::new();
    let mut add = |id, group, icon, title: String, line: String, (state, button): Gate| {
        out.push(Row {
            id,
            group,
            icon,
            title,
            line,
            state,
            button,
        });
    };

    // ---- Share and files ----
    if sends(PACKET_TYPE_SHARE_REQUEST) {
        add(
            "send",
            Group::Share,
            "desklink-upload-file-symbolic",
            "Send files".into(),
            format!("Drop files on this window, or choose them. They land on {name}."),
            on_phone(Action::SendFiles, "Choose files"),
        );
        add(
            "link",
            Group::Share,
            "desklink-link-symbolic",
            "Send a link".into(),
            format!("A web page you send opens on {name}."),
            on_phone(Action::SendLink, "Send a link"),
        );
    }
    if sends(PACKET_TYPE_FOLDERS) {
        add(
            "bucket",
            Group::Share,
            "desklink-directory-sync-symbolic",
            "Bucket".into(),
            format!("A shared space: anything you put in it is on {name} too."),
            gate(Feature::Send(PACKET_TYPE_FOLDERS)).unwrap_or_else(|| {
                if facts.bucket {
                    open("Open Files", Target::Page("files"))
                } else {
                    (
                        State::Step("It is off".into()),
                        Some((
                            "Turn on".into(),
                            Go::Do(Do::Fix(Fix::TurnOnBucket(phone.id.clone()))),
                        )),
                    )
                }
            }),
        );
    }
    add(
        "photos",
        Group::Share,
        "desklink-photo-library-symbolic",
        format!("Photos from {name}"),
        format!("New photos and screenshots from {name} show up in Files."),
        if facts.photos {
            open("Open Files", Target::Page("files"))
        } else {
            turned_off("Files")
        },
    );
    add(
        "files",
        Group::Share,
        "desklink-folder-symbolic",
        "Sent and received".into(),
        "Every file sent both ways, and where it was saved.".into(),
        open("Open Files", Target::Page("files")),
    );

    // ---- Screen and control ----
    if sends(PACKET_TYPE_MOUSEPAD_REQUEST) {
        let screen = gate(Feature::Send(PACKET_TYPE_MOUSEPAD_REQUEST));
        add(
            "phone-screen",
            Group::Screen,
            "desklink-mobile-screen-share-symbolic",
            format!("{} screen", facts.kind),
            format!("See and control {name} in a window here. The phone asks you first."),
            screen
                .clone()
                .or_else(|| need("plugins"))
                .unwrap_or_else(|| on_phone(Action::PhoneScreen, &format!("Show {kind} screen"))),
        );
        add(
            "insert",
            Group::Screen,
            "desklink-photo-camera-symbolic",
            format!("Insert from {name}"),
            "A photo, a scan, text or a QR code from the phone's camera, straight into this \
             computer."
                .into(),
            screen
                .clone()
                .unwrap_or_else(|| ready("Choose what", Go::Insert)),
        );
        add(
            "sound",
            Group::Screen,
            "desklink-volume-up-symbolic",
            "Sound both ways".into(),
            format!(
                "Play {name}'s sound here, this computer's sound on {name}, or use {name} as a \
                 microphone."
            ),
            screen.unwrap_or_else(|| ready("Sound options", Go::Sound)),
        );
    }
    add(
        "desk-screen",
        Group::Screen,
        "desklink-desktop-windows-symbolic",
        format!("This screen on {name}"),
        format!(
            "On {name}, tap Desk screen to watch this screen and use the phone as its trackpad \
             and keyboard."
        ),
        broken("share")
            .or_else(|| need("plugins"))
            .or_else(|| need("screen-control"))
            .unwrap_or_else(|| open("Control settings", guide::settings("Control"))),
    );
    add(
        "extend",
        Group::Screen,
        "desklink-devices-symbolic",
        format!("{} as a second screen", facts.kind),
        format!("On {name}, open Desk screen and choose Extend, then drag windows onto it."),
        broken("share")
            .or_else(|| broken("extend"))
            .unwrap_or_else(|| {
                if facts.displays {
                    ready("Arrange displays", Go::Do(Do::Displays))
                } else {
                    open("Control settings", guide::settings("Control"))
                }
            }),
    );
    if sends(PACKET_TYPE_SHARE_INPUT_DEVICES_REQUEST) {
        add(
            "glide",
            Group::Screen,
            "desklink-mouse-symbolic",
            "Glide".into(),
            format!("Push the mouse past the edge of this screen and it keeps going onto {name}."),
            glide(facts, phone, &needs),
        );
    }

    // ---- Phone and calls ----
    add(
        "notifications",
        Group::Phone,
        "desklink-notifications-symbolic",
        format!("{name}'s notifications"),
        "They show up here. Reply to them or dismiss them from this computer.".into(),
        need("notifications")
            .or_else(|| need("phone-notification-access"))
            .unwrap_or_else(|| open("Open notifications", Target::Page("notifications"))),
    );
    if sends(PACKET_TYPE_NOTIFICATION) {
        add(
            "forward",
            Group::Phone,
            "desklink-notifications-active-symbolic",
            format!("This computer's alerts on {name}"),
            format!("Notifications from apps here show on {name} too."),
            gate(Feature::Send(PACKET_TYPE_NOTIFICATION)).unwrap_or_else(|| {
                if facts.forward {
                    open("Notification settings", guide::settings("Notifications"))
                } else {
                    turned_off("Notifications")
                }
            }),
        );
    }
    if sends(PACKET_TYPE_TELEPHONY_REQUEST) {
        add(
            "calls",
            Group::Phone,
            "desklink-call-symbolic",
            "Calls".into(),
            format!("Answer and decline {name}'s calls here, and dial a number with it."),
            gate(Feature::Send(PACKET_TYPE_TELEPHONY_REQUEST))
                .unwrap_or_else(|| open("Dial a number", Target::Action(Action::Dial))),
        );
    }
    if sends(PACKET_TYPE_CONTACTS_REQUEST_ALL_UIDS_TIMESTAMPS) {
        add(
            "contacts",
            Group::Phone,
            "desklink-person-symbolic",
            "Contacts".into(),
            format!("{name}'s contacts, to look up, call or copy."),
            open("Open contacts", Target::Page("contacts")),
        );
    }
    if sends(PACKET_TYPE_FINDMYPHONE_REQUEST) {
        add(
            "ring",
            Group::Phone,
            "desklink-ring-volume-symbolic",
            "Find my phone".into(),
            format!("Make {name} ring so you can find it, even when it is on silent."),
            on_phone(Action::Ring, &format!("Ring {kind}")),
        );
    }
    if sends(PACKET_TYPE_LOCK_REQUEST) {
        add(
            "lock-phone",
            Group::Phone,
            "desklink-lock-symbolic",
            format!("Lock {kind}"),
            format!("Lock {name}'s screen from here."),
            on_phone(Action::Lock, &format!("Lock {kind}")),
        );
    }
    if sends(PACKET_TYPE_PING) {
        add(
            "nudge",
            Group::Phone,
            "desklink-vibration-symbolic",
            "Send a nudge".into(),
            format!("A quiet notification on {name}."),
            on_phone(Action::Nudge, "Send a nudge"),
        );
    }
    if sends(PACKET_TYPE_MPRIS_REQUEST) {
        add(
            "music",
            Group::Phone,
            "desklink-music-note-symbolic",
            "Music".into(),
            format!(
                "Play, pause and skip {name}'s music from here. Its controls show at the top of \
                 Home while it plays."
            ),
            gate(Feature::Send(PACKET_TYPE_MPRIS_REQUEST))
                .unwrap_or_else(|| ready("Play or pause", Go::Music(facts.player.clone()))),
        );
    }

    // ---- Notes and clipboard ----
    if sends(PACKET_TYPE_CLIPBOARD) {
        add(
            "clipboard",
            Group::Notes,
            "desklink-content-paste-symbolic",
            "Copy on one, paste on the other".into(),
            "With clipboard sharing on, what you copy on one is ready to paste on the other."
                .into(),
            if facts.clipboard_sync {
                on_phone(Action::SendClipboard, "Send clipboard now")
            } else {
                turned_off("Clipboard")
            },
        );
    }
    add(
        "clip-history",
        Group::Notes,
        "desklink-history-symbolic",
        "Clipboard history".into(),
        "What was copied on both, to copy again.".into(),
        open("Open history", Target::Page("clipboard")),
    );
    add(
        "notes",
        Group::Notes,
        "desklink-edit-note-symbolic",
        "Notes on both".into(),
        format!("Write a note here and it is on {name} too, and the other way round."),
        // The guide's button: the Notes page would open on whichever of its
        // tabs was last shown.
        open("New note", Target::Action(Action::NewNote)),
    );
    add(
        "snippets",
        Group::Notes,
        "desklink-keep-symbolic",
        "Snippets".into(),
        "Bits of text you keep, to paste again on either one.".into(),
        open("Open snippets", Target::Page("snippets")),
    );

    // ---- This computer ----
    add(
        "terminals",
        Group::Computer,
        "desklink-terminal-symbolic",
        "Terminals on both".into(),
        format!("Start a shared terminal here or on {name}, and carry on from either one."),
        need("terminal-access")
            .unwrap_or_else(|| open("Open terminals", Target::Page("terminals"))),
    );
    add(
        "hand-off",
        Group::Computer,
        "desklink-lock-open-symbolic",
        "Hand off".into(),
        format!("Lock this computer and carry on in your newest terminal on {name}."),
        on_phone(Action::HandOff, "Hand off"),
    );
    add(
        "lock",
        Group::Computer,
        "desklink-lock-symbolic",
        "Lock and unlock".into(),
        format!("Lock this computer from {name}, and unlock it with {name} once that is on."),
        broken("lock")
            .or_else(|| need("unlock"))
            .unwrap_or_else(|| open("Privacy & security", guide::settings("Privacy & security"))),
    );
    add(
        "walk-away",
        Group::Computer,
        "desklink-front-hand-symbolic",
        "Lock when you walk away".into(),
        format!("This computer locks when {name} has been out of reach for a minute."),
        broken("lock").unwrap_or_else(|| {
            if facts.walk_away {
                open("Privacy & security", guide::settings("Privacy & security"))
            } else {
                turned_off("Privacy & security")
            }
        }),
    );
    add(
        "top-bar",
        Group::Computer,
        "desklink-more-horiz-symbolic",
        "Top-bar menu".into(),
        "DeskLink in GNOME's top bar: send files, ring or lock in one click.".into(),
        match facts.base.extension {
            None => (
                State::Off("Only on GNOME. Here, use this window or DeskLink's tray icon.".into()),
                None,
            ),
            Some(ExtensionStatus::Active) => {
                open("Appearance settings", guide::settings("Appearance"))
            }
            Some(_) => need("extension")
                .unwrap_or_else(|| open("Appearance settings", guide::settings("Appearance"))),
        },
    );
    add(
        "start-at-login",
        Group::Computer,
        "desklink-restart-alt-symbolic",
        "Start at login".into(),
        format!("So {name} can reach this computer after a restart."),
        need("start-at-login")
            .unwrap_or_else(|| open("Connection settings", guide::settings("Connection"))),
    );
    add(
        "widgets",
        Group::Computer,
        "desklink-device-phone-symbolic",
        "Widgets and Quick Settings tiles".into(),
        format!(
            "On {name}: touch and hold the home screen to add DeskLink widgets, and edit Quick \
             Settings to add its tiles."
        ),
        (State::OnPhone, None),
    );

    // ---- Help ----
    add(
        "pair",
        Group::Help,
        "desklink-qr-code-scanner-symbolic",
        "Pair another phone".into(),
        "Add a phone or tablet: this computer shows a code to scan.".into(),
        open("Show a code", Target::Action(Action::PairAnother)),
    );
    add(
        "guide",
        Group::Help,
        "desklink-info-symbolic",
        "Get started".into(),
        "A short guide to each feature, one card each.".into(),
        open("Open the guide", Target::Action(Action::Guide)),
    );
    add(
        "trouble",
        Group::Help,
        "desklink-wifi-symbolic",
        "Can't connect?".into(),
        "Put both on the same Wi-Fi and keep DeskLink open on the phone.".into(),
        if phone.reach == Reach::Offline {
            ready(
                "Try again",
                Go::Do(Do::Fix(Fix::Reconnect(phone.id.clone()))),
            )
        } else {
            ready("Search for phones", Go::Do(Do::Discover))
        },
    );
    add(
        "history",
        Group::Help,
        "desklink-schedule-symbolic",
        "Connection history".into(),
        "When your phones connected.".into(),
        open("Open history", Target::Action(Action::History)),
    );
    add(
        "shortcuts",
        Group::Help,
        "desklink-keyboard-symbolic",
        "Keyboard shortcuts".into(),
        "Every key that does something.".into(),
        open("Show shortcuts", Target::Action(Action::Shortcuts)),
    );
    out
}

/// Glide's state, every one of them: this computer cannot capture the
/// mouse; the phone is away or too old; off here (turn it on); the phone is
/// locked or said no (unlock it, or ask it); the desktop's dialog is up or
/// was refused (turn on again); some other problem; ready.
fn glide(facts: &Facts, phone: &PhoneFacts, needs: &[Need]) -> Gate {
    if let Availability::Unavailable(why) = facts.glide {
        return (State::Off(why.to_string()), None);
    }
    if let Some(gate) = blocker(
        Feature::Send(PACKET_TYPE_SHARE_INPUT_DEVICES_REQUEST),
        phone,
    ) {
        return from_need(&gate);
    }
    if !phone.glide_here {
        return (
            State::Step("It is off on this computer".into()),
            Some((
                "Turn on Glide".into(),
                Go::Do(Do::Fix(Fix::TurnOnGlide(phone.id.clone()))),
            )),
        );
    }
    let settings = || open("Glide settings", guide::settings("Control"));
    if let Some(need) = needs
        .iter()
        .find(|need| need.id == "glide-phone" || need.id == "glide")
    {
        return from_need(need);
    }
    match &facts.glide_problem {
        Some(problem) => (State::Step(problem.clone()), settings().1),
        None => settings(),
    }
}

/// Words that change only when a row's state or button does: the Home
/// page's key, so the section is never rebuilt under the pointer for
/// nothing.
pub(super) fn key(rows: &[Row]) -> String {
    rows.iter()
        .map(|row| {
            format!(
                "{}:{}:{}",
                row.id,
                row.state.key(),
                row.button.as_ref().map_or("", |(words, _)| words)
            )
        })
        .collect::<Vec<_>>()
        .join("|")
}

// ---- Drawing -------------------------------------------------------------------

fn label(text: &str, classes: &[&str]) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .xalign(0.0)
        .css_classes(classes)
        .build()
}

/// Does what a row's button says.
fn go(button: &gtk::Button, what: &Go, daemon: &DaemonHandle, device: &DeviceView) {
    match what {
        // The tiles' own menus, on this button.
        Go::Insert => super::insert::attach_menu(daemon, device, button),
        Go::Sound => super::sound::attach_menu(daemon, device, button),
        Go::Do(what) => {
            let what = what.clone();
            button.connect_clicked(move |button| {
                if let Some(window) = button.root().and_downcast::<DesklinkWindow>() {
                    guide::run(&window, &what);
                }
            });
        }
        Go::Music(player) => {
            let (daemon, id, name, player) = (
                daemon.clone(),
                device.id.clone(),
                device.name.clone(),
                player.clone(),
            );
            button.connect_clicked(move |button| {
                let Some(window) = button.root().and_downcast::<DesklinkWindow>() else {
                    return;
                };
                match &player {
                    Some(player) => {
                        daemon.send(DaemonCommand::SendMprisAction(
                            id.clone(),
                            player.clone(),
                            "PlayPause".into(),
                        ));
                        window.show_feature_toast(&format!("Play or pause sent to {name}"));
                    }
                    None => window.show_feature_toast(&format!(
                        "Play music on {name} and its controls show at the top of Home"
                    )),
                }
            });
        }
    }
}

fn row_widget(row: &Row, daemon: &DaemonHandle, device: &DeviceView) -> gtk::ListBoxRow {
    let tone = match row.state {
        State::Ready | State::OnPhone => Tone::Accent,
        State::Step(_) => Tone::Warn,
        State::Off(_) => Tone::Neutral,
    };
    let square = ui_kit::icon_square(row.icon, tone);
    square.set_valign(gtk::Align::Start);
    let text = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(4)
        .hexpand(true)
        .build();
    text.append(&label(&row.title, &["dl-need-title"]));
    text.append(&label(&row.line, &["dl-secondary"]));
    let (status, word) = row.state.pill();
    let mut spoken = format!("{}. {word}.", row.title);
    if let State::Step(why) | State::Off(why) = &row.state {
        let note = gtk::Box::builder()
            .spacing(6)
            .css_classes(["dl-guide-not-now"])
            .build();
        note.append(
            &gtk::Image::builder()
                .icon_name("desklink-info-symbolic")
                .pixel_size(16)
                .valign(gtk::Align::Start)
                .accessible_role(gtk::AccessibleRole::Presentation)
                .build(),
        );
        // An install command is read and copied whole.
        let words = label(why, &[]);
        words.set_selectable(matches!(row.state, State::Step(_)) && why.contains("sudo "));
        note.append(&words);
        text.append(&note);
        spoken = format!("{spoken} {why}");
    }
    // The pill and the button wrap under each other when the row is narrow.
    let end = gtk::FlowBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .homogeneous(false)
        .max_children_per_line(2)
        .column_spacing(10)
        .row_spacing(6)
        .halign(gtk::Align::Start)
        .margin_top(4)
        .build();
    if let Some((words, what)) = &row.button {
        let button = ui_kit::button(ButtonKind::Tonal, words, None);
        button.set_widget_name(&format!("everything-{}", row.id));
        button.update_property(&[
            gtk::accessible::Property::Label(&format!("{words}: {}", row.title)),
            gtk::accessible::Property::Description(&spoken),
        ]);
        go(&button, what, daemon, device);
        super::transfers::add_card(&end, &button);
    }
    let pill = ui_kit::status_pill(status, word);
    super::transfers::add_card(&end, &pill);
    text.append(&end);
    let content = gtk::Box::builder()
        .spacing(12)
        .css_classes(["dl-need"])
        .build();
    content.append(&square);
    content.append(&text);
    let widget = gtk::ListBoxRow::builder()
        .activatable(false)
        .focusable(false)
        .child(&content)
        .build();
    widget.set_widget_name(row.id);
    widget.update_property(&[gtk::accessible::Property::Label(&spoken)]);
    widget
}

/// The section: its title, then each group's rows in a card.
pub(super) fn section(daemon: &DaemonHandle, device: &DeviceView, rows: &[Row]) -> gtk::Box {
    let section = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .margin_top(16)
        .css_classes(["dl-everything"])
        .build();
    section.set_widget_name("everything");
    let title = label(TITLE, &["dl-card-title"]);
    title.set_accessible_role(gtk::AccessibleRole::Heading);
    title.update_property(&[gtk::accessible::Property::Level(2)]);
    section.append(&title);
    section.append(&label(SUBTITLE, &["dl-secondary"]));
    for group in Group::ALL {
        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["dl-row-group", "dl-needs"])
            .build();
        for row in rows.iter().filter(|row| row.group == group) {
            list.append(&row_widget(row, daemon, device));
        }
        if list.first_child().is_none() {
            continue;
        }
        let header = ui_kit::section_header(group.title());
        header.set_margin_top(12);
        list.update_property(&[gtk::accessible::Property::Label(group.title())]);
        section.append(&header);
        section.append(&list);
    }
    section
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device_links::packet::PACKET_TYPE_SETTINGS_REQUEST;
    use crate::platform::{evaluate, Distro, Probe};

    fn phone() -> PhoneFacts {
        PhoneFacts {
            id: "p".into(),
            name: "Pixel 9".into(),
            reach: Reach::Ready,
            incoming: crate::device_links::features::FeatureRegistry::desktop()
                .outgoing_capabilities()
                .to_vec(),
            terminal_access: true,
            unlock_on: true,
            control_approved: true,
            glide_here: true,
            notification_access: Some(true),
            ..PhoneFacts::default()
        }
    }

    /// A set-up computer (the probe of `requirements`' tests) and phone,
    /// with everything turned on.
    fn facts() -> Facts {
        let probe = Probe {
            desktop: "GNOME".into(),
            session_type: Some("wayland".into()),
            wayland_display: true,
            session_bus: true,
            system_bus: true,
            runtime_dir: true,
            session_names: [
                "org.freedesktop.portal.Desktop",
                "org.freedesktop.Notifications",
            ]
            .map(String::from)
            .into(),
            system_names: ["org.freedesktop.login1".to_string()].into(),
            portals: ["ScreenCast", "RemoteDesktop", "InputCapture"]
                .map(String::from)
                .into(),
            input_capture: Some(3),
            elements: [
                "webrtcbin",
                "dtlsdec",
                "dtlssrtpenc",
                "nicesrc",
                "opusenc",
                "opusdec",
                "vp8dec",
                "vp8enc",
                "pipewiresrc",
                "pipewiresink",
            ]
            .map(String::from)
            .into(),
            pipewire_socket: true,
            config_writable: true,
            ..Probe::default()
        };
        Facts {
            base: requirements::Facts {
                phone: Some(phone()),
                checks: evaluate(&probe),
                distro: Some(Distro::Arch),
                start_at_login: true,
                extension: Some(ExtensionStatus::Active),
                glide_can_ask_again: false,
                glide_asking: false,
            },
            kind: "Phone".into(),
            glide: Availability::Available,
            glide_problem: None,
            clipboard_sync: true,
            photos: true,
            forward: true,
            walk_away: true,
            bucket: true,
            player: Some("Spotify".into()),
            displays: true,
        }
    }

    fn with_phone(change: impl FnOnce(&mut PhoneFacts)) -> Facts {
        let mut facts = facts();
        change(facts.base.phone.as_mut().unwrap());
        facts
    }

    fn row(facts: &Facts, id: &str) -> Row {
        rows(facts)
            .into_iter()
            .find(|row| row.id == id)
            .unwrap_or_else(|| panic!("no {id} row"))
    }

    /// The row a Find anything action is found in. Exhaustive: a new action
    /// must be given a row.
    fn row_of(action: Action) -> &'static str {
        match action {
            Action::SendFiles => "send",
            Action::SendLink => "link",
            Action::SendClipboard => "clipboard",
            Action::Ring => "ring",
            Action::Nudge => "nudge",
            Action::Lock => "lock-phone",
            Action::PhoneScreen => "phone-screen",
            Action::Insert(_) => "insert",
            Action::Sound(..) => "sound",
            Action::HandOff => "hand-off",
            Action::NewNote => "notes",
            Action::NewTerminal => "terminals",
            Action::Dial => "calls",
            Action::PairAnother => "pair",
            Action::History => "history",
            Action::Shortcuts => "shortcuts",
            Action::Guide => "guide",
        }
    }

    /// Every Get started card and every Find anything action has its row,
    /// once, in a group; and a phone action that is ready does exactly what
    /// the palette's row does.
    #[test]
    fn every_feature_of_the_guide_and_the_palette_has_a_row() {
        let facts = facts();
        let rows = rows(&facts);
        let ids: Vec<&str> = rows.iter().map(|row| row.id).collect();
        let mut unique = ids.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), ids.len(), "{ids:?}");
        let cards = guide::cards(&guide::Facts {
            phone: facts.base.phone.clone(),
            extension: Some(ExtensionStatus::Off),
            displays: true,
            firewall: None,
        });
        for card in &cards {
            assert!(ids.contains(&card.id), "the guide's {} card", card.id);
        }
        let palette = super::super::palette_model::PhoneState {
            id: "p".into(),
            name: "Pixel 9".into(),
            kind: "phone".into(),
            usable: true,
            files: true,
            clipboard: true,
            ring: true,
            ping: true,
            lock: true,
            screen: true,
            captures: vec![0, 1],
            ..Default::default()
        };
        for action in super::super::palette_model::offered_actions(Some(&palette)) {
            let id = row_of(action);
            let row = rows
                .iter()
                .find(|row| row.id == id)
                .unwrap_or_else(|| panic!("{action:?}: no {id} row"));
            if let Some(Do::Run(Target::PhoneAction(_, sent))) =
                row.button.as_ref().and_then(|(_, go)| match go {
                    Go::Do(what) => Some(what),
                    _ => None,
                })
            {
                assert_eq!(feature_of(*sent), feature_of(action), "{id}");
            }
        }
        for group in Group::ALL {
            assert!(rows.iter().any(|row| row.group == group), "{group:?}");
        }
        assert!(rows.len() >= 30, "{}", rows.len());
    }

    /// No dead row, whatever the state: ready has a button that goes
    /// somewhere real, a step says what it is, what is not available says
    /// why and has no button. Plain words throughout.
    #[test]
    fn no_row_is_dead_in_any_state() {
        let mut broken = facts();
        broken.base.checks = evaluate(&Probe {
            desktop: "sway".into(),
            wayland_display: true,
            session_bus: true,
            config_writable: true,
            ..Probe::default()
        });
        broken.base.distro = Some(Distro::Debian);
        broken.base.extension = None;
        broken.glide = Availability::Unavailable("This desktop cannot share the mouse");
        let mut off = facts();
        off.clipboard_sync = false;
        off.photos = false;
        off.forward = false;
        off.walk_away = false;
        off.bucket = false;
        off.player = None;
        off.base.start_at_login = false;
        off.base.extension = Some(ExtensionStatus::Off);
        let states = [
            facts(),
            broken,
            off,
            with_phone(|p| p.reach = Reach::Offline),
            with_phone(|p| p.reach = Reach::Connecting),
            with_phone(|p| p.incoming.clear()),
            with_phone(|p| {
                p.terminal_access = false;
                p.unlock_on = false;
                p.control_approved = false;
                p.glide_here = false;
                p.notification_access = Some(false);
            }),
        ];
        for facts in &states {
            assert!(rows(facts).len() >= 30, "every state lists every feature");
            for row in rows(facts) {
                let id = row.id;
                match &row.state {
                    State::Ready => assert!(row.button.is_some(), "{id}: ready, no button"),
                    State::Step(words) => assert!(!words.is_empty(), "{id}"),
                    State::Off(why) => {
                        assert!(!why.is_empty(), "{id}");
                        assert!(row.button.is_none(), "{id}: a dead button");
                    }
                    State::OnPhone => assert!(row.line.starts_with("On "), "{id}"),
                }
                if let Some((words, go)) = &row.button {
                    super::super::onboarding::assert_plain(words);
                    match go {
                        Go::Do(Do::Run(Target::Settings(index))) => {
                            assert!(*index < super::super::prefs::GROUPS.len());
                            // 0 is also what a misspelt name falls back to.
                            assert!(
                                *index != 0 || id == "start-at-login",
                                "{id}: a named page, not the first"
                            );
                        }
                        Go::Do(Do::Run(Target::Page(page))) => {
                            let (page, _) = super::super::resolve_page(page);
                            assert!(
                                super::super::PAGES.iter().any(|(name, _, _)| *name == page),
                                "{id}: {page}"
                            )
                        }
                        Go::Do(Do::Fix(fix)) => assert!(fix.label().is_some(), "{id}"),
                        _ => {}
                    }
                }
                super::super::onboarding::assert_plain(&row.title);
                super::super::onboarding::assert_plain(&row.line);
            }
        }
    }

    /// A phone action: ready does what its tile does; away it is Try again;
    /// an older phone says to update it, with no button.
    #[test]
    fn a_phone_action_says_what_it_needs() {
        let send = row(&facts(), "send");
        assert_eq!(send.state, State::Ready);
        assert_eq!(
            send.button,
            Some((
                "Choose files".into(),
                Go::Do(Do::Run(Target::PhoneAction("p".into(), Action::SendFiles)))
            ))
        );
        let away = row(&with_phone(|p| p.reach = Reach::Offline), "send");
        assert_eq!(away.state, State::Step("Can't reach Pixel 9".into()));
        assert_eq!(
            away.button,
            Some((
                "Try again".into(),
                Go::Do(Do::Fix(Fix::Reconnect("p".into())))
            ))
        );
        let connecting = row(&with_phone(|p| p.reach = Reach::Connecting), "send");
        assert!(
            matches!(connecting.state, State::Off(_)),
            "waits, no button"
        );
        let old = row(&with_phone(|p| p.incoming.clear()), "ring");
        let State::Off(why) = &old.state else {
            panic!("{:?}", old.state)
        };
        assert!(why.contains("Update DeskLink on Pixel 9"), "{why}");
        assert_eq!(old.button, None);

        // Off by choice: the step turns it on (the Bucket at once).
        let mut off = facts();
        off.bucket = false;
        off.clipboard_sync = false;
        assert_eq!(
            row(&off, "bucket").button.unwrap().1,
            Go::Do(Do::Fix(Fix::TurnOnBucket("p".into())))
        );
        assert_eq!(
            row(&off, "clipboard").button.unwrap().1,
            Go::Do(Do::Run(guide::settings("Clipboard")))
        );
        // A step the phone takes: the words, no button.
        let access = row(
            &with_phone(|p| p.notification_access = Some(false)),
            "notifications",
        );
        let State::Step(words) = &access.state else {
            panic!("{:?}", access.state)
        };
        assert!(words.contains("Notification access"), "{words}");
        assert_eq!(access.button, None);
        // Insert and Sound open their tiles' menus.
        assert_eq!(row(&facts(), "insert").button.unwrap().1, Go::Insert);
        assert_eq!(row(&facts(), "sound").button.unwrap().1, Go::Sound);
    }

    /// Glide's row follows every state, with the one action that helps.
    #[test]
    fn glide_says_every_state_with_its_action() {
        let glide = |facts: &Facts| {
            let row = row(facts, "glide");
            (row.state, row.button)
        };
        let fix = |words: &str, fix: Fix| Some((words.to_string(), Go::Do(Do::Fix(fix))));

        // Ready.
        assert_eq!(
            glide(&facts()),
            (
                State::Ready,
                Some((
                    "Glide settings".into(),
                    Go::Do(Do::Run(guide::settings("Control")))
                ))
            )
        );
        // This computer cannot capture the mouse: why, no button.
        let mut unsupported = facts();
        unsupported.glide = Availability::Unavailable("This desktop cannot share the mouse");
        assert_eq!(
            glide(&unsupported),
            (
                State::Off("This desktop cannot share the mouse".into()),
                None
            )
        );
        // Off here: turn it on.
        assert_eq!(
            glide(&with_phone(|p| p.glide_here = false)),
            (
                State::Step("It is off on this computer".into()),
                fix("Turn on Glide", Fix::TurnOnGlide("p".into()))
            )
        );
        // The phone said no: ask it (it takes the ask).
        let said_no = with_phone(|p| p.glide_off_there = true);
        assert!(phone()
            .incoming
            .iter()
            .any(|t| t == PACKET_TYPE_SETTINGS_REQUEST));
        assert_eq!(
            glide(&said_no).1,
            fix("Ask on phone", Fix::AskPhoneGlide("p".into()))
        );
        // Locked: unlock it, nothing to click here.
        let locked = glide(&with_phone(|p| {
            p.glide_off_there = true;
            p.glide_locked = true;
        }));
        let State::Step(words) = &locked.0 else {
            panic!("{locked:?}")
        };
        assert!(words.starts_with("Unlock Pixel 9 to use Glide"), "{words}");
        assert_eq!(locked.1, None);
        // The desktop's dialog is up: answer it.
        let mut asking = facts();
        asking.base.glide_asking = true;
        let (State::Step(words), None) = glide(&asking) else {
            panic!("{:?}", glide(&asking))
        };
        assert!(words.contains("Capture Input"), "{words}");
        // Refused or unanswered: turn on again.
        let mut paused = facts();
        paused.base.glide_can_ask_again = true;
        paused.glide_problem = Some("Off for now because you said no.".into());
        assert_eq!(glide(&paused).1, fix("Turn on again", Fix::AskGlide));
        // Some other problem: said, with its settings.
        let mut stuck = facts();
        stuck.glide_problem = Some("Glide stopped working".into());
        assert_eq!(glide(&stuck).0, State::Step("Glide stopped working".into()));
        // Away, or an older phone.
        assert_eq!(
            glide(&with_phone(|p| p.reach = Reach::Offline)).1,
            fix("Try again", Fix::Reconnect("p".into()))
        );
        assert!(matches!(
            glide(&with_phone(|p| p.incoming.clear())),
            (State::Off(_), None)
        ));
    }

    /// The key moves with a state or a button, nothing else.
    #[test]
    fn the_key_follows_the_states() {
        let same = key(&rows(&facts()));
        assert_eq!(same, key(&rows(&facts())));
        assert_ne!(same, key(&rows(&with_phone(|p| p.glide_here = false))));
        assert_ne!(same, key(&rows(&with_phone(|p| p.reach = Reach::Offline))));
    }

    // ---- On Home (the dedicated test display) ---------------------------------

    fn pump() {
        let context = gtk::glib::MainContext::default();
        for _ in 0..30 {
            while context.iteration(false) {}
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    /// Home ends with the section: a row for each feature, and a button
    /// that does something: Turn on Glide sends the setting and opens
    /// Settings; New note opens a note to type in.
    #[test]
    fn home_ends_with_every_feature_and_each_button_acts() {
        use adw::subclass::prelude::ObjectSubclassIsExt;
        crate::test_display::run(|| {
            let resources = crate::load_resources().expect("the bundled resources");
            gtk::gio::resources_register(&resources);
            let app = adw::Application::builder()
                .application_id("derx06.desklink.com.test")
                .build();
            let window = DesklinkWindow::new(&app);
            let (daemon, commands) = DaemonHandle::detached();
            daemon.set_load_state_for_tests(crate::device_links::daemon::handle::LOAD_KNOWN);
            window.set_daemon(daemon);
            let phone = super::super::debug_fake_device("paired", "Pixel 9");
            let id = phone.id.clone();
            window.imp().fake_devices.borrow_mut().push(phone.clone());
            window.select_device(&id);
            window.present();
            pump();
            window.refresh_devices();
            pump();
            let root: gtk::Widget = window.clone().upcast();
            let section = super::super::named_widgets(&root, "everything")
                .into_iter()
                .find(|w| w.widget_name() == "everything")
                .expect("the section on Home");
            let expected = rows(&Facts::now(requirements::Facts::now(Some(&phone)), &phone));
            for row in &expected {
                assert!(
                    super::super::named_widgets(&section, row.id)
                        .iter()
                        .any(|w| w.widget_name() == row.id),
                    "{} is listed",
                    row.id
                );
                if row.button.is_some() {
                    assert!(
                        !super::super::named_widgets(&section, &format!("everything-{}", row.id))
                            .is_empty(),
                        "{}: its button",
                        row.id
                    );
                }
            }
            let button = |id: &str| {
                super::super::named_widgets(&section, &format!("everything-{id}"))
                    .into_iter()
                    .next()
                    .and_downcast::<gtk::Button>()
            };
            // Glide is off for a fresh phone: the button turns it on.
            let glide = expected.iter().find(|row| row.id == "glide");
            // Off here; or, once another test started Glide's worker on this
            // display, not available (no Wayland), with no button.
            assert!(
                matches!(
                    glide,
                    Some(Row {
                        button: Some((_, Go::Do(Do::Fix(Fix::TurnOnGlide(_))))),
                        ..
                    }) | Some(Row {
                        state: State::Off(_),
                        button: None,
                        ..
                    })
                ),
                "{glide:?}"
            );
            if let Some(Row {
                button: Some((_, Go::Do(Do::Fix(Fix::TurnOnGlide(_))))),
                ..
            }) = glide
            {
                button("glide").expect("Turn on Glide").emit_clicked();
                pump();
                let sent: Vec<_> = commands.try_iter().collect();
                assert!(
                    sent.iter()
                        .any(|c| matches!(c, DaemonCommand::SetGlide(d, Some(_)) if *d == id)),
                    "{sent:?}"
                );
                assert_eq!(window.imp().page.borrow().as_str(), "settings");
                window.show_page("home");
                pump();
            }
            let section = super::super::named_widgets(&window.clone().upcast(), "everything")
                .into_iter()
                .find(|w| w.widget_name() == "everything")
                .expect("the section on Home");
            super::super::named_widgets(&section, "everything-notes")
                .into_iter()
                .next()
                .and_downcast::<gtk::Button>()
                .expect("New note")
                .emit_clicked();
            pump();
            assert_eq!(
                window.imp().page.borrow().as_str(),
                "notes",
                "a new note, on the Notes page"
            );
            window.close();
        });
    }
}
