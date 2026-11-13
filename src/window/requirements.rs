//! What is still missing, for this computer and the selected phone, and the
//! one fix for each: the one list behind Home's "Finish setting up" card,
//! Settings' "Finish setting up" group, and the reason every unavailable
//! tile, menu row and palette action gives when it is clicked.
//!
//! [`missing`] and [`blocker`] are pure: the facts come in a [`Facts`], so a
//! test fakes any computer and phone. Phone-side items are only what the
//! phone reports (today its notification access); nothing here sends a
//! packet the protocol does not already have.

use adw::prelude::*;

use crate::device_links::daemon::{DaemonCommand, DaemonHandle};
use crate::device_links::device::{DeviceStatus, DeviceView};
use crate::platform::{Check, Distro, Level};
use crate::shell_dbus::ExtensionStatus;

/// What turning on the top bar menu also does (`shell_dbus::turn_on_extension`
/// sets `disable-user-extensions` to false), said wherever it can be turned on.
pub(crate) const EXTENSION_NOTE: &str = "Turning it on also allows GNOME user extensions \
    (a setting for all of them), and GNOME shows it after you log out and back in.";

/// The Settings subpage of passwords, terminals and unlocking.
const PRIVACY_PAGE: usize = 7;

/// What a need's button does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Fix {
    /// Nothing to click here: the words say what to do.
    None,
    StartAtLogin,
    /// A Settings subpage (an index into `prefs::GROUPS`).
    OpenSettings(usize),
    /// Glide asks the desktop's permission again.
    AskGlide,
    TurnOnExtension,
    LogOut,
    /// Approve the phone controlling this screen, now, at the desk.
    ApproveControl(String),
    /// Ask the phone to turn Glide on there (a prompt on the phone).
    AskPhoneGlide(String),
    Reconnect(String),
    /// Copies an install command.
    Copy(String),
}

impl Fix {
    /// The button's words; `None` when there is no button.
    pub(crate) fn label(&self) -> Option<&'static str> {
        Some(match self {
            Self::None => return None,
            Self::StartAtLogin => "Turn on",
            Self::OpenSettings(PRIVACY_PAGE) => "Open Privacy & security",
            Self::OpenSettings(_) => "Open Settings",
            Self::AskGlide => "Turn on again",
            Self::TurnOnExtension => "Turn on",
            Self::LogOut => "Log out…",
            Self::ApproveControl(_) => "Approve now",
            Self::AskPhoneGlide(_) => "Ask on phone",
            Self::Reconnect(_) => "Try again",
            Self::Copy(_) => "Copy command",
        })
    }
}

/// What "Allow all on this computer" does for a need (after the person
/// confirms the list).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Allow {
    StartAtLogin,
    TerminalAccess(String),
    Unlock(String),
    Extension,
    AskGlide,
}

/// One thing that is missing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Need {
    pub id: &'static str,
    /// Done on the phone, not here.
    pub on_phone: bool,
    /// What is missing, in one line: "Terminal access is off for Pixel 9".
    pub title: String,
    /// What it unlocks, or what to do where there is no button.
    pub detail: String,
    pub fix: Fix,
    /// What "Allow all on this computer" does about it, if anything.
    pub allow: Option<Allow>,
}

/// How far the phone is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Reach {
    Ready,
    Connecting,
    #[default]
    Offline,
}

/// The selected phone, as far as the needs go.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct PhoneFacts {
    pub id: String,
    pub name: String,
    pub reach: Reach,
    /// What its build takes (`incomingCapabilities`).
    pub incoming: Vec<String>,
    pub terminal_access: bool,
    pub unlock_on: bool,
    /// Control of this screen approved at the desk (until DeskLink restarts).
    pub control_approved: bool,
    /// Glide is set to this phone.
    pub glide_here: bool,
    /// The phone said Glide is off there (its switch for this computer, or
    /// the accessibility service it needs).
    pub glide_off_there: bool,
    /// Android lets it read notifications (`None`: an older phone, unknown).
    pub notification_access: Option<bool>,
}

impl PhoneFacts {
    pub(crate) fn from_device(device: &DeviceView) -> Self {
        let settings = crate::device_links::config::app_settings();
        Self {
            id: device.id.clone(),
            name: device.name.clone(),
            reach: match device.status {
                DeviceStatus::Paired if device.feature_transport_ready => Reach::Ready,
                DeviceStatus::Paired => Reach::Connecting,
                _ => Reach::Offline,
            },
            incoming: device.incoming_capabilities.clone(),
            terminal_access: device.terminal_access,
            unlock_on: settings.unlocks_from(&device.id),
            control_approved: crate::device_links::config::remote_desktop_token(&device.id)
                .is_some(),
            glide_here: settings
                .glide
                .as_ref()
                .is_some_and(|glide| glide.device == device.id),
            glide_off_there: crate::device_links::glide::phone_allows(&device.id) == Some(false),
            notification_access: device.notification_access,
        }
    }
}

/// Everything [`missing`] reads.
#[derive(Debug, Clone, Default)]
pub(crate) struct Facts {
    pub phone: Option<PhoneFacts>,
    /// The capability probe's rows (empty until it has run).
    pub checks: Vec<Check>,
    pub distro: Option<Distro>,
    pub start_at_login: bool,
    /// DeskLink's top-bar extension; `None` where GNOME is not running.
    pub extension: Option<ExtensionStatus>,
    /// Glide waits for the person instead of asking again.
    pub glide_can_ask_again: bool,
    /// The desktop's input-capture dialog is up now.
    pub glide_asking: bool,
}

impl Facts {
    /// This computer and `device`, as they are now.
    pub(crate) fn now(device: Option<&DeviceView>) -> Self {
        let glide = crate::device_links::glide::status();
        Self {
            phone: device
                .filter(|device| device.trusted)
                .map(PhoneFacts::from_device),
            checks: crate::platform::report()
                .map(|rows| rows.to_vec())
                .unwrap_or_default(),
            distro: crate::platform::distro(),
            start_at_login: crate::device_links::config::app_settings()
                .start_at_login
                .unwrap_or(true),
            extension: crate::shell_dbus::extension_possible()
                .then(crate::shell_dbus::extension_status),
            glide_can_ask_again: glide.can_ask_again,
            glide_asking: glide.asking,
        }
    }

    fn check(&self, id: &str) -> Option<&Check> {
        self.checks.iter().find(|check| check.id == id)
    }

    fn install(&self, check: &Check) -> Option<String> {
        check
            .install
            .filter(|_| check.level != Level::Ok)
            .map(|packages| packages.command(self.distro))
    }
}

/// Everything still missing, this computer's first, then the phone's, in a
/// fixed order. Empty when all is set up.
pub(crate) fn missing(facts: &Facts) -> Vec<Need> {
    let mut needs = Vec::new();
    let copy = |check: &Check| facts.install(check).map_or(Fix::None, Fix::Copy);

    if !facts.start_at_login {
        needs.push(Need {
            id: "start-at-login",
            on_phone: false,
            title: "Start at login is off".into(),
            detail: "Your phone can reach this computer after a restart only if DeskLink starts \
                     at login."
                .into(),
            fix: Fix::StartAtLogin,
            allow: Some(Allow::StartAtLogin),
        });
    }
    if let Some(check) = facts
        .check("notifications")
        .filter(|check| check.level != Level::Ok)
    {
        needs.push(Need {
            id: "notifications",
            on_phone: false,
            title: "This desktop cannot show notifications".into(),
            detail: format!(
                "Your phone's notifications and DeskLink's alerts need one to show here.{}",
                facts
                    .install(check)
                    .map(|command| format!("\nInstall: {command}"))
                    .unwrap_or_default()
            ),
            fix: copy(check),
            allow: None,
        });
    }
    let plugins: Vec<&Check> = ["link", "view", "sound"]
        .iter()
        .filter_map(|id| facts.check(id))
        .filter(|check| check.level != Level::Ok)
        .collect();
    if !plugins.is_empty() {
        let mut commands: Vec<String> = Vec::new();
        for command in plugins.iter().filter_map(|check| facts.install(check)) {
            if !commands.contains(&command) {
                commands.push(command);
            }
        }
        let detail = plugins
            .iter()
            .map(|check| check.detail.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        needs.push(Need {
            id: "plugins",
            on_phone: false,
            title: "Video and sound support is missing".into(),
            detail: if commands.is_empty() {
                detail
            } else {
                format!("{detail}\nInstall: {}", commands.join("; "))
            },
            fix: if commands.is_empty() {
                Fix::None
            } else {
                Fix::Copy(commands.join("\n"))
            },
            allow: None,
        });
    }
    match facts.extension {
        Some(ExtensionStatus::Off) => needs.push(Need {
            id: "extension",
            on_phone: false,
            title: "The top bar menu is off".into(),
            detail: format!(
                "Your phone's battery, connection and quick actions in GNOME's top bar. {}",
                EXTENSION_NOTE
            ),
            fix: Fix::TurnOnExtension,
            allow: Some(Allow::Extension),
        }),
        Some(ExtensionStatus::NeedsLogin) => needs.push(Need {
            id: "extension",
            on_phone: false,
            title: "The top bar menu needs you to log in again".into(),
            detail: "It is installed. GNOME shows it after you log out and back in.".into(),
            fix: Fix::LogOut,
            allow: None,
        }),
        _ => {}
    }

    let Some(phone) = &facts.phone else {
        return needs;
    };
    let name = &phone.name;
    if !phone.terminal_access {
        needs.push(Need {
            id: "terminal-access",
            on_phone: false,
            title: format!("Terminal access is off for {name}"),
            detail: format!("Open and share terminals from {name}, and use Hand off."),
            fix: Fix::OpenSettings(PRIVACY_PAGE),
            allow: Some(Allow::TerminalAccess(phone.id.clone())),
        });
    }
    if !phone.unlock_on
        && facts
            .check("lock")
            .is_some_and(|check| check.level == Level::Ok)
    {
        needs.push(Need {
            id: "unlock",
            on_phone: false,
            title: format!("Unlock with {name} is off"),
            detail: format!(
                "Unlock this computer from {name} while both are on this network; you approve \
                 it on the phone each time."
            ),
            fix: Fix::OpenSettings(PRIVACY_PAGE),
            allow: Some(Allow::Unlock(phone.id.clone())),
        });
    }
    if !phone.control_approved
        && facts
            .check("share")
            .is_some_and(|check| check.level == Level::Ok)
    {
        needs.push(Need {
            id: "screen-control",
            on_phone: false,
            title: format!("Control from {name} is not approved yet"),
            detail: format!(
                "Approve it once here, with Allow Remote Interaction on, and {name} can see and \
                 control this screen with nobody at the computer, until DeskLink restarts."
            ),
            fix: Fix::ApproveControl(phone.id.clone()),
            allow: None,
        });
    }
    if phone.glide_here && phone.glide_off_there {
        needs.push(glide_off_there(phone));
    }
    if phone.glide_here && facts.glide_asking {
        needs.push(Need {
            id: "glide",
            on_phone: false,
            title: "Glide is asking for permission".into(),
            detail: "Allow it in the Capture Input dialog to move your mouse across to the phone."
                .into(),
            fix: Fix::None,
            allow: None,
        });
    } else if phone.glide_here && facts.glide_can_ask_again {
        needs.push(Need {
            id: "glide",
            on_phone: false,
            title: "Glide is paused".into(),
            detail: format!(
                "It needs your OK on this computer again to move your mouse across to {name}. \
                 Turn it on again and allow the Capture Input dialog."
            ),
            fix: Fix::AskGlide,
            allow: Some(Allow::AskGlide),
        });
    }
    if phone.notification_access == Some(false) {
        needs.push(Need {
            id: "phone-notification-access",
            on_phone: true,
            title: format!("On {name}: allow notification access"),
            detail: format!(
                "So {name}'s notifications show here. On the phone, open DeskLink, then \
                 Settings, Permissions, Notification access."
            ),
            fix: Fix::None,
            allow: None,
        });
    }
    needs
}

/// Glide is on here but off on the phone: what to do there, and "Ask on
/// phone" when its build can show the prompt (an older one: the words only).
fn glide_off_there(phone: &PhoneFacts) -> Need {
    let name = &phone.name;
    let askable = crate::device_links::glide::can_ask_phone(&phone.incoming);
    Need {
        id: "glide-phone",
        on_phone: true,
        title: format!("Glide is off on {name}. Turn it on there."),
        detail: if askable {
            format!(
                "{name} shows a prompt when you ask; or on {name}, open DeskLink, then Settings, \
                 Control, and turn on control from this computer."
            )
        } else {
            format!(
                "On {name}, open DeskLink, then Settings, Control, and turn on control from this \
                 computer."
            )
        },
        fix: if askable {
            Fix::AskPhoneGlide(phone.id.clone())
        } else {
            Fix::None
        },
        allow: None,
    }
}

/// What an unavailable action is blocked by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Feature {
    /// Anything sent to the phone as this packet type.
    Send(&'static str),
    /// Lock here and carry on in the newest terminal on the phone.
    HandOff,
}

/// Why `feature` cannot run on `phone` now, as a need with its fix; `None`
/// when it can. Reachability first, then what the phone's build takes, then
/// the feature's own requirements. No open terminal does not block Hand off:
/// it offers to start one ([`super::overview::hand_off`]).
pub(crate) fn blocker(feature: Feature, phone: &PhoneFacts) -> Option<Need> {
    let name = &phone.name;
    match phone.reach {
        Reach::Offline => {
            return Some(Need {
                id: "reach",
                on_phone: false,
                title: format!("Can't reach {name}"),
                detail: format!(
                    "Check that {name} is on, with DeskLink open, and on your network."
                ),
                fix: Fix::Reconnect(phone.id.clone()),
                allow: None,
            })
        }
        Reach::Connecting => {
            return Some(Need {
                id: "reach",
                on_phone: false,
                title: format!("Connecting to {name}…"),
                detail: "This works as soon as the phone is connected.".into(),
                fix: Fix::None,
                allow: None,
            })
        }
        Reach::Ready => {}
    }
    match feature {
        Feature::Send(packet) if !phone.incoming.iter().any(|known| known == packet) => {
            Some(Need {
                id: "phone-update",
                on_phone: true,
                title: format!("{name}'s DeskLink can't do this yet"),
                detail: format!("Update DeskLink on {name} to use it."),
                fix: Fix::None,
                allow: None,
            })
        }
        Feature::Send(_) => None,
        Feature::HandOff if !phone.terminal_access => Some(Need {
            id: "terminal-access",
            on_phone: false,
            title: format!("Needs terminal access for {name}"),
            detail: format!("Hand off opens your newest terminal on {name}, so it needs access."),
            fix: Fix::OpenSettings(PRIVACY_PAGE),
            allow: Some(Allow::TerminalAccess(phone.id.clone())),
        }),
        Feature::HandOff => None,
    }
}

/// The short words a tile's hint line shows for `need`.
pub(crate) fn short(need: &Need) -> &'static str {
    match need.id {
        "reach" if need.fix == Fix::None => "Connecting…",
        "reach" => "Can't reach",
        "phone-update" => "Needs an update",
        "terminal-access" => "Needs access",
        _ => "Needs a step",
    }
}

// ---- Doing it ------------------------------------------------------------------

/// Runs `need`'s fix from `anchor` (a widget in the main window).
pub(crate) fn run(anchor: &impl IsA<gtk::Widget>, need: &Need) {
    let Some(window) = anchor.root().and_downcast::<super::DesklinkWindow>() else {
        return;
    };
    let daemon = window.daemon();
    match &need.fix {
        Fix::None => {}
        Fix::StartAtLogin => {
            if let Some(daemon) = daemon {
                daemon.send(DaemonCommand::SetStartAtLogin(true));
            }
            window.show_feature_toast("Start at login turned on");
        }
        Fix::OpenSettings(index) => {
            super::prefs::show_group(*index);
            if !window.show_page("settings") {
                if let Some(app) = window.application() {
                    app.activate_action("preferences", None);
                }
            }
        }
        Fix::AskGlide => {
            crate::device_links::glide::ask_again();
            window.show_feature_toast("Glide will ask for permission");
        }
        Fix::TurnOnExtension => match crate::shell_dbus::turn_on_extension() {
            Ok(true) => window.show_feature_toast("Top bar menu installed. Log out to see it"),
            Ok(false) => window.show_feature_toast("Top bar menu turned on"),
            Err(error) => window.show_feature_toast(&format!(
                "Could not turn on the top bar menu{}",
                super::because(&error)
            )),
        },
        Fix::LogOut => crate::shell_dbus::log_out(),
        Fix::ApproveControl(id) => super::prefs::approve_remote_control(&window, id),
        Fix::AskPhoneGlide(id) => {
            crate::device_links::glide::ask_phone(id);
            window.show_feature_toast("Asked on the phone. Turn Glide on there");
        }
        Fix::Reconnect(id) => {
            if let Some(daemon) = daemon {
                daemon.send(DaemonCommand::Reconnect(id.clone()));
            }
        }
        Fix::Copy(command) => {
            window.clipboard().set_text(command);
            window.show_feature_toast("Command copied");
        }
    }
    window.refresh_soon();
}

/// Says why something is unavailable: the need in a dialog, with its fix as
/// the suggested response.
pub(crate) fn explain(anchor: &impl IsA<gtk::Widget>, need: &Need) {
    let dialog = adw::AlertDialog::new(Some(&need.title), Some(&need.detail));
    dialog.add_response(
        "close",
        if need.fix.label().is_some() {
            "Not now"
        } else {
            "OK"
        },
    );
    dialog.set_close_response("close");
    if let Some(label) = need.fix.label() {
        dialog.add_response("fix", label);
        dialog.set_response_appearance("fix", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("fix"));
    }
    let (need, anchor_weak) = (need.clone(), anchor.as_ref().downgrade());
    dialog.connect_response(Some("fix"), move |_, _| {
        if let Some(anchor) = anchor_weak.upgrade() {
            run(&anchor, &need);
        }
    });
    dialog.present(Some(anchor));
}

/// What "Allow all on this computer" will do, one line each, for the dialog
/// that asks first.
pub(crate) fn allow_all_lines(needs: &[Need]) -> Vec<String> {
    needs
        .iter()
        .filter_map(|need| {
            Some(match need.allow.as_ref()? {
                Allow::StartAtLogin => "Start at login".to_string(),
                Allow::TerminalAccess(_) | Allow::Unlock(_) => need
                    .title
                    .replace(" is off for ", " for ")
                    .replace(" is off", ""),
                Allow::Extension => {
                    "The top bar menu, which also allows GNOME user extensions (it shows after \
                     you log in again)"
                        .into()
                }
                Allow::AskGlide => "Glide asks the desktop for permission".into(),
            })
        })
        .collect()
}

/// "Allow all on this computer": asks once, listing what it turns on, then
/// does each.
pub(crate) fn allow_all(anchor: &impl IsA<gtk::Widget>, daemon: &DaemonHandle, needs: &[Need]) {
    let allows: Vec<Allow> = needs.iter().filter_map(|need| need.allow.clone()).collect();
    if allows.is_empty() {
        return;
    }
    let body = allow_all_lines(needs)
        .iter()
        .map(|line| format!("• {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    let dialog = adw::AlertDialog::new(
        Some("Allow all on this computer?"),
        Some(&format!("DeskLink turns these on:\n{body}")),
    );
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("allow", "Allow all");
    dialog.set_response_appearance("allow", adw::ResponseAppearance::Suggested);
    dialog.set_close_response("cancel");
    let daemon = daemon.clone();
    let anchor_weak = anchor.as_ref().downgrade();
    dialog.connect_response(Some("allow"), move |_, _| {
        for allow in &allows {
            match allow {
                Allow::StartAtLogin => daemon.send(DaemonCommand::SetStartAtLogin(true)),
                Allow::TerminalAccess(id) => {
                    daemon.send(DaemonCommand::SetTerminalAccess(id.clone(), true))
                }
                Allow::Unlock(id) => daemon.send(DaemonCommand::SetUnlockNearby(id.clone(), true)),
                Allow::Extension => {
                    if let Err(error) = crate::shell_dbus::turn_on_extension() {
                        eprintln!("[UI] Could not turn on the top-bar menu: {error}");
                    }
                }
                Allow::AskGlide => crate::device_links::glide::ask_again(),
            }
        }
        if let Some(window) = anchor_weak
            .upgrade()
            .and_then(|anchor| anchor.root())
            .and_downcast::<super::DesklinkWindow>()
        {
            window.show_feature_toast("Turned on");
            window.refresh_soon();
        }
    });
    dialog.present(Some(anchor));
}

/// The needs as rows: icon, what is missing, what it unlocks, and the fix
/// under the words (so a narrow window or large text keeps the words wide).
pub(crate) fn rows(needs: &[Need]) -> gtk::ListBox {
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["dl-row-group", "dl-needs"])
        .build();
    for need in needs {
        let (icon, tone) = if need.on_phone {
            (
                "desklink-device-phone-symbolic",
                super::ui_kit::Tone::Accent,
            )
        } else {
            ("desklink-warning-symbolic", super::ui_kit::Tone::Warn)
        };
        let square = super::ui_kit::icon_square(icon, tone);
        square.set_valign(gtk::Align::Start);
        let text = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(4)
            .hexpand(true)
            .build();
        let line = |words: &str, classes: &[&str]| {
            gtk::Label::builder()
                .label(words)
                .wrap(true)
                .wrap_mode(gtk::pango::WrapMode::WordChar)
                .xalign(0.0)
                .css_classes(classes)
                .build()
        };
        text.append(&line(&need.title, &["dl-need-title"]));
        let detail = line(&need.detail, &["dl-secondary"]);
        // An install command is read and copied whole.
        detail.set_selectable(true);
        text.append(&detail);
        if let Some(label) = need.fix.label() {
            let button = super::ui_kit::button(super::ui_kit::ButtonKind::Tonal, label, None);
            button.set_halign(gtk::Align::Start);
            button.set_margin_top(4);
            button.update_property(&[gtk::accessible::Property::Description(&need.title)]);
            let need = need.clone();
            button.connect_clicked(move |button| run(button, &need));
            text.append(&button);
        }
        let content = gtk::Box::builder()
            .spacing(12)
            .css_classes(["dl-need"])
            .build();
        content.append(&square);
        content.append(&text);
        let row = gtk::ListBoxRow::builder()
            .activatable(false)
            .focusable(false)
            .child(&content)
            .build();
        row.set_widget_name(need.id);
        row.update_property(&[gtk::accessible::Property::Label(&need.title)]);
        list.append(&row);
    }
    list
}

thread_local! {
    /// The Home card folded away (for this run).
    static FOLDED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Home's "Finish setting up" card: every need, a fix on each, "Allow all on
/// this computer" when any can be; `None` when nothing is missing. It can be
/// folded to its title line.
pub(crate) fn card(daemon: &DaemonHandle, needs: &[Need]) -> Option<gtk::Box> {
    if needs.is_empty() {
        return None;
    }
    let card = super::ui_kit::card(None);
    card.set_widget_name("finish-setup");
    let head = gtk::Box::builder().spacing(12).build();
    let title = gtk::Label::builder()
        .label("Finish setting up")
        .xalign(0.0)
        .hexpand(true)
        .wrap(true)
        .css_classes(["dl-card-title"])
        .accessible_role(gtk::AccessibleRole::Heading)
        .build();
    head.append(&title);
    let count = gtk::Label::builder()
        .label(if needs.len() == 1 {
            "1 step left".to_string()
        } else {
            format!("{} steps left", needs.len())
        })
        .css_classes(["dl-secondary"])
        .build();
    head.append(&count);
    let folded = FOLDED.with(|cell| cell.get());
    let fold = super::ui_kit::icon_button(
        if folded {
            "desklink-expand-more-symbolic"
        } else {
            "desklink-expand-less-symbolic"
        },
        if folded {
            "Show the steps"
        } else {
            "Hide the steps"
        },
    );
    head.append(&fold);
    card.append(&head);
    let body = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .visible(!folded)
        .build();
    body.append(&rows(needs));
    if needs.iter().any(|need| need.allow.is_some()) {
        let all = super::ui_kit::button(
            super::ui_kit::ButtonKind::Primary,
            "Allow all on this computer",
            None,
        );
        all.set_halign(gtk::Align::Start);
        let (daemon, needs) = (daemon.clone(), needs.to_vec());
        all.connect_clicked(move |button| allow_all(button, &daemon, &needs));
        body.append(&all);
    }
    card.append(&body);
    fold.connect_clicked(move |fold| {
        let folded = body.is_visible();
        body.set_visible(!folded);
        FOLDED.with(|cell| cell.set(folded));
        fold.set_icon_name(if folded {
            "desklink-expand-more-symbolic"
        } else {
            "desklink-expand-less-symbolic"
        });
        let words = if folded {
            "Show the steps"
        } else {
            "Hide the steps"
        };
        fold.set_tooltip_text(Some(words));
        fold.update_property(&[gtk::accessible::Property::Label(words)]);
    });
    Some(card)
}

/// The ids of `needs`, for a page's redraw key.
pub(crate) fn key(needs: &[Need]) -> String {
    needs
        .iter()
        .map(|need| need.id)
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device_links::packet::{PACKET_TYPE_LOCK_REQUEST, PACKET_TYPE_PING};
    use crate::platform::{evaluate, Probe};
    use gtk::{gio, glib};

    fn phone() -> PhoneFacts {
        PhoneFacts {
            id: "p1".into(),
            name: "Pixel 9".into(),
            reach: Reach::Ready,
            incoming: vec![PACKET_TYPE_PING.into()],
            terminal_access: true,
            unlock_on: true,
            control_approved: true,
            glide_here: false,
            glide_off_there: false,
            notification_access: Some(true),
        }
    }

    /// Glide on here, off on the phone: said on the phone's line with the
    /// ask when the phone's build takes it, the instruction alone when not;
    /// nothing when Glide is not set to this phone or is on there.
    #[test]
    fn glide_off_on_the_phone_says_so_and_asks_when_it_can() {
        let facts = |phone: PhoneFacts| Facts {
            phone: Some(phone),
            ..complete()
        };
        let glide_need = |phone: PhoneFacts| {
            missing(&facts(phone))
                .into_iter()
                .find(|need| need.id == "glide-phone")
        };
        let mut off = phone();
        off.glide_here = true;
        off.glide_off_there = true;
        let need = glide_need(off.clone()).expect("listed");
        assert_eq!(need.title, "Glide is off on Pixel 9. Turn it on there.");
        assert!(need.on_phone);
        assert_eq!(need.fix, Fix::None, "an older phone: the words only");
        assert!(need.detail.contains("Settings, Control"));
        off.incoming
            .push(crate::device_links::packet::PACKET_TYPE_SETTINGS_REQUEST.into());
        let need = glide_need(off.clone()).expect("listed");
        assert_eq!(need.fix, Fix::AskPhoneGlide("p1".into()));
        assert_eq!(need.fix.label(), Some("Ask on phone"));
        assert!(need.allow.is_none(), "never switched from here");

        let mut on_there = off.clone();
        on_there.glide_off_there = false;
        assert_eq!(glide_need(on_there), None);
        let mut not_here = off;
        not_here.glide_here = false;
        assert_eq!(glide_need(not_here), None);
    }

    fn complete() -> Facts {
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
            phone: Some(phone()),
            checks: evaluate(&probe),
            distro: Some(Distro::Arch),
            start_at_login: true,
            extension: Some(ExtensionStatus::Active),
            glide_can_ask_again: false,
            glide_asking: false,
        }
    }

    fn ids(needs: &[Need]) -> Vec<&'static str> {
        needs.iter().map(|need| need.id).collect()
    }

    #[test]
    fn nothing_is_missing_on_a_set_up_computer_and_phone() {
        assert!(
            missing(&complete()).is_empty(),
            "{:?}",
            missing(&complete())
        );
    }

    /// Every missing piece, in order, each with what it unlocks and its one
    /// fix; Allow all covers only what is safe to turn on from here.
    #[test]
    fn every_missing_piece_is_listed_with_its_fix() {
        let mut facts = complete();
        facts.start_at_login = false;
        facts.extension = Some(ExtensionStatus::Off);
        facts.glide_can_ask_again = true;
        let phone = facts.phone.as_mut().unwrap();
        phone.terminal_access = false;
        phone.unlock_on = false;
        phone.control_approved = false;
        phone.glide_here = true;
        phone.notification_access = Some(false);
        let needs = missing(&facts);
        assert_eq!(
            ids(&needs),
            [
                "start-at-login",
                "extension",
                "terminal-access",
                "unlock",
                "screen-control",
                "glide",
                "phone-notification-access"
            ]
        );
        let by = |id| needs.iter().find(|need| need.id == id).unwrap();
        assert!(
            by("extension")
                .detail
                .contains("also allows GNOME user extensions"),
            "{}",
            by("extension").detail
        );
        assert_eq!(by("start-at-login").fix, Fix::StartAtLogin);
        assert_eq!(by("terminal-access").fix, Fix::OpenSettings(PRIVACY_PAGE));
        assert_eq!(
            by("terminal-access").fix.label(),
            Some("Open Privacy & security")
        );
        assert_eq!(by("screen-control").fix, Fix::ApproveControl("p1".into()));
        assert_eq!(by("glide").fix, Fix::AskGlide);
        let phone_side = by("phone-notification-access");
        assert!(phone_side.on_phone);
        assert!(phone_side.title.starts_with("On Pixel 9: allow"));
        assert_eq!(phone_side.fix, Fix::None, "no packet asks the phone");
        assert!(phone_side.detail.contains("Settings, Permissions"));
        assert_eq!(
            needs
                .iter()
                .filter_map(|need| need.allow.clone())
                .collect::<Vec<_>>(),
            [
                Allow::StartAtLogin,
                Allow::Extension,
                Allow::TerminalAccess("p1".into()),
                Allow::Unlock("p1".into()),
                Allow::AskGlide
            ]
        );
        assert_eq!(
            allow_all_lines(&needs),
            [
                "Start at login",
                "The top bar menu, which also allows GNOME user extensions (it shows after you \
                 log in again)",
                "Terminal access for Pixel 9",
                "Unlock with Pixel 9",
                "Glide asks the desktop for permission"
            ]
        );
        for need in &needs {
            assert!(!need.detail.is_empty(), "{}", need.id);
            assert!(!need.title.contains('!'));
        }
    }

    #[test]
    fn a_broken_computer_says_what_to_install() {
        let facts = Facts {
            checks: evaluate(&Probe {
                desktop: "sway".into(),
                wayland_display: true,
                session_bus: true,
                config_writable: true,
                ..Probe::default()
            }),
            distro: Some(Distro::Debian),
            start_at_login: true,
            ..Facts::default()
        };
        let needs = missing(&facts);
        assert_eq!(ids(&needs), ["notifications", "plugins"]);
        let Fix::Copy(command) = &needs[1].fix else {
            panic!("{:?}", needs[1].fix)
        };
        assert!(command.starts_with("sudo apt install"), "{command}");
    }

    /// An unavailable action says why and what fixes it: reachability first,
    /// then the phone's build, then Hand off's own needs.
    #[test]
    fn a_blocked_action_names_its_reason_and_fix() {
        let ping = Feature::Send(PACKET_TYPE_PING);
        let lock = Feature::Send(PACKET_TYPE_LOCK_REQUEST);
        assert_eq!(blocker(ping, &phone()), None);
        let need = blocker(lock, &phone()).unwrap();
        assert_eq!(need.id, "phone-update");
        assert!(need.detail.contains("Update DeskLink on Pixel 9"));
        assert_eq!(short(&need), "Needs an update");

        let mut offline = phone();
        offline.reach = Reach::Offline;
        let need = blocker(ping, &offline).unwrap();
        assert_eq!(need.fix, Fix::Reconnect("p1".into()));
        assert_eq!(short(&need), "Can't reach");
        offline.reach = Reach::Connecting;
        assert_eq!(short(&blocker(ping, &offline).unwrap()), "Connecting…");

        // No open terminal is not a blocker: Hand off offers to start one.
        assert_eq!(blocker(Feature::HandOff, &phone()), None);
        let mut closed = phone();
        closed.terminal_access = false;
        let need = blocker(Feature::HandOff, &closed).unwrap();
        assert_eq!(need.fix, Fix::OpenSettings(PRIVACY_PAGE));
        assert_eq!(short(&need), "Needs access");
    }

    #[test]
    fn the_card_is_gone_when_nothing_is_missing() {
        crate::test_display::run(|| {
            let (daemon, _commands) = DaemonHandle::detached();
            assert!(card(&daemon, &[]).is_none());
            let mut facts = complete();
            facts.start_at_login = false;
            let needs = missing(&facts);
            let card = card(&daemon, &needs).expect("one step left");
            assert_eq!(card.widget_name(), "finish-setup");
        });
    }

    fn find(root: &gtk::Widget, test: &dyn Fn(&gtk::Widget) -> bool) -> Option<gtk::Widget> {
        let mut stack = vec![root.clone()];
        while let Some(widget) = stack.pop() {
            if test(&widget) {
                return Some(widget);
            }
            let mut child = widget.first_child();
            while let Some(next) = child {
                child = next.next_sibling();
                stack.push(next);
            }
        }
        None
    }

    /// Home with a phone that has no terminal access: the "Finish setting
    /// up" card lists it with its fix, and the Hand off tile is listed,
    /// looks unavailable, and a click says why with the fix (it does not
    /// lock anything).
    #[test]
    fn home_lists_what_is_missing_and_an_unavailable_tile_explains_itself() {
        use adw::subclass::prelude::ObjectSubclassIsExt;
        crate::test_display::run(|| {
            let resources = crate::load_resources().expect("the bundled resources");
            gio::resources_register(&resources);
            let app = adw::Application::builder()
                .application_id("derx06.desklink.com.test")
                .build();
            let window = super::super::DesklinkWindow::new(&app);
            let (daemon, _commands) = DaemonHandle::detached();
            window.set_daemon(daemon);
            let phone = super::super::debug_fake_device("paired-new", "Pixel 9");
            assert!(!phone.terminal_access);
            let id = phone.id.clone();
            window.imp().fake_devices.borrow_mut().push(phone);
            window.select_device(&id);
            window.present();
            let context = glib::MainContext::default();
            for _ in 0..30 {
                while context.iteration(false) {}
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            window.refresh_devices();
            let root: gtk::Widget = window.clone().upcast();
            let card = find(&root, &|w| {
                w.widget_name() == "finish-setup" && w.is_mapped()
            })
            .expect("the Finish setting up card");
            assert!(
                find(&card, &|w| w.widget_name() == "terminal-access").is_some(),
                "terminal access is listed"
            );
            let hand_off = find(&root, &|w| {
                w.has_css_class("dl-unavailable")
                    && w.is_mapped()
                    && w.downcast_ref::<gtk::Button>().is_some_and(|button| {
                        find(button.upcast_ref(), &|l| {
                            l.downcast_ref::<gtk::Label>()
                                .is_some_and(|l| l.label() == "Hand off")
                        })
                        .is_some()
                    })
            })
            .and_downcast::<gtk::Button>()
            .expect("Hand off is listed, unavailable");
            assert!(hand_off.is_sensitive(), "clickable, not inert grey");
            assert_eq!(hand_off.action_name(), None, "nothing to send");
            hand_off.emit_clicked();
            let dialog = window
                .visible_dialog()
                .and_downcast::<adw::AlertDialog>()
                .expect("a click says why");
            assert_eq!(
                dialog.heading().as_deref(),
                Some("Needs terminal access for Pixel 9")
            );
            assert!(dialog.has_response("fix"));
            assert_eq!(
                dialog.response_label("fix").as_str(),
                "Open Privacy & security"
            );
            dialog.close();
        });
    }
}
