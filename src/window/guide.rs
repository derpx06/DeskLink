//! "Get started": what DeskLink does, one card each, with the button that
//! does it or opens the right place. The phone's Get started guide is the
//! same list in the same words, said from the phone's side.
//!
//! [`cards`] is pure (the facts come in a [`Facts`]); [`show`] draws them in
//! a dialog and [`run`] maps a button onto what the palette, the setup rows
//! and the menu already do. A card that cannot be used now says why, with
//! the fix as its button: nothing is hidden.

use adw::prelude::*;
#[cfg(test)]
use gtk::gio;
use gtk::glib;

use super::palette_model::{feature_of, Action, Target};
use super::requirements::{self, Feature, Fix, PhoneFacts, Reach};
use super::ui_kit::{self, ButtonKind, Tone};
use super::DesklinkWindow;
use crate::shell_dbus::ExtensionStatus;

pub(super) const TITLE: &str = "Get started";
pub(super) const SUBTITLE: &str = "What DeskLink can do, and how to start each one.";

/// What a card's button does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Do {
    /// The same as the palette's row ([`super::palette::run`]).
    Run(Target),
    /// A setup row's fix ([`requirements::run`]).
    Fix(Fix),
    /// Look for phones on this network (the menu's "Search for phones").
    Discover,
    /// The desktop's display settings, where a second screen is placed.
    Displays,
}

/// One card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Card {
    pub id: &'static str,
    pub icon: &'static str,
    pub title: &'static str,
    /// How, in a line or two.
    pub how: String,
    /// Why it cannot be used now; the buttons are then the fix.
    pub not_now: Option<String>,
    pub buttons: Vec<(String, Do)>,
}

/// Everything [`cards`] reads.
#[derive(Debug, Clone, Default)]
pub(super) struct Facts {
    /// The selected paired phone.
    pub phone: Option<PhoneFacts>,
    /// DeskLink's top-bar menu; `None` where GNOME is not running.
    pub extension: Option<ExtensionStatus>,
    /// The desktop's display settings can be opened.
    pub displays: bool,
    /// The command that lets DeskLink through this computer's firewall.
    pub firewall: Option<String>,
}

/// What a card needs from the phone.
enum Needs {
    Nothing,
    /// A paired phone.
    Pair,
    /// A paired phone that is reachable and takes `Action`.
    Ready(Action),
}

/// Settings at the subpage called `name` (one of `prefs::GROUPS`).
pub(super) fn settings(name: &str) -> Target {
    Target::Settings(
        super::prefs::GROUPS
            .iter()
            .position(|group| *group == name)
            .unwrap_or(0),
    )
}

fn pair_button(paired: bool) -> (String, Do) {
    let words = if paired {
        "Pair another phone"
    } else {
        "Pair a phone"
    };
    (words.into(), Do::Run(Target::Action(Action::PairAnother)))
}

/// Why a card cannot be used now, with its fix, or `None`.
fn not_now(phone: Option<&PhoneFacts>, needs: &Needs) -> Option<(String, Vec<(String, Do)>)> {
    let action = match needs {
        Needs::Nothing => return None,
        _ if phone.is_none() => {
            return Some(("Pair a phone first.".into(), vec![pair_button(false)]))
        }
        Needs::Pair => return None,
        Needs::Ready(action) => *action,
    };
    let phone = phone?;
    let need = match feature_of(action) {
        Some(feature) => requirements::blocker(feature, phone),
        // Reachability is judged before the feature.
        None if phone.reach != Reach::Ready => requirements::blocker(Feature::HandOff, phone),
        None => None,
    }?;
    let gap = if need.title.ends_with(['…', '.']) {
        " "
    } else {
        ". "
    };
    let buttons = need
        .fix
        .label()
        .map(|label| vec![(label.to_string(), Do::Fix(need.fix.clone()))])
        .unwrap_or_default();
    Some((format!("{}{gap}{}", need.title, need.detail), buttons))
}

/// The cards, in the phone's order.
pub(super) fn cards(facts: &Facts) -> Vec<Card> {
    let phone = facts.phone.as_ref();
    let name = phone.map_or("your phone", |phone| phone.name.as_str());
    let on_phone = |action: Action| -> Vec<(String, Do)> {
        phone.map_or_else(Vec::new, |phone| {
            let words = match action {
                Action::SendFiles => "Choose files",
                Action::PhoneScreen => "Show phone screen",
                _ => "Ring phone",
            };
            vec![(
                words.to_string(),
                Do::Run(Target::PhoneAction(phone.id.clone(), action)),
            )]
        })
    };
    let mut out = Vec::new();
    let mut add = |id, icon, title, how: String, needs: Needs, buttons: Vec<(String, Do)>| {
        let (not_now, buttons) = match not_now(phone, &needs) {
            Some((why, fix)) => (Some(why), fix),
            None => (None, buttons),
        };
        out.push(Card {
            id,
            icon,
            title,
            how,
            not_now,
            buttons,
        });
    };

    add(
        "pair",
        "desklink-qr-code-scanner-symbolic",
        "Pair a phone",
        match phone {
            Some(phone) => format!(
                "Paired with {}. To add another phone or tablet, show a new code.",
                phone.name
            ),
            None => "Install DeskLink on your phone, open it, and scan the code this \
                     computer shows."
                .into(),
        },
        Needs::Nothing,
        vec![pair_button(phone.is_some())],
    );
    add(
        "send",
        "desklink-upload-file-symbolic",
        "Send a file",
        format!("Drop files on this window, or choose them here. They land on {name}."),
        Needs::Ready(Action::SendFiles),
        on_phone(Action::SendFiles),
    );
    add(
        "clipboard",
        "desklink-content-paste-symbolic",
        "Copy on one, paste on the other",
        "Turn on clipboard sharing, then copy on one and paste on the other.".into(),
        Needs::Pair,
        vec![("Clipboard settings".into(), Do::Run(settings("Clipboard")))],
    );
    add(
        "desk-screen",
        "desklink-desktop-windows-symbolic",
        "See your computer's screen on the phone",
        format!(
            "On {name}, tap Desk screen to watch this screen and use the phone as its \
             trackpad and keyboard."
        ),
        Needs::Pair,
        vec![("Control settings".into(), Do::Run(settings("Control")))],
    );
    add(
        "extend",
        "desklink-devices-symbolic",
        "Use the phone as a second screen",
        format!("On {name}, open Desk screen and choose Extend, then drag windows onto it."),
        Needs::Pair,
        vec![if facts.displays {
            ("Arrange displays".into(), Do::Displays)
        } else {
            ("Control settings".into(), Do::Run(settings("Control")))
        }],
    );
    add(
        "phone-screen",
        "desklink-mobile-screen-share-symbolic",
        "Share your phone's screen to the computer",
        format!("Show {name}'s screen in a window here. The phone asks you first."),
        Needs::Ready(Action::PhoneScreen),
        on_phone(Action::PhoneScreen),
    );
    let reads = phone.and_then(|phone| phone.notification_access) != Some(false);
    add(
        "notifications",
        "desklink-notifications-symbolic",
        "Phone notifications on the computer",
        if reads {
            format!("Notifications from {name} show up here. Reply to them or dismiss them from this computer.")
        } else {
            format!("Notifications from {name} show up here once {name} allows it. The Notifications page asks {name} for you.")
        },
        Needs::Pair,
        vec![(
            "Open notifications".into(),
            Do::Run(Target::Page("notifications")),
        )],
    );
    add(
        "terminals",
        "desklink-terminal-symbolic",
        "Terminals on both",
        "Start a shared terminal here or on the phone, and carry on from either one.".into(),
        Needs::Nothing,
        vec![(
            "New shared terminal".into(),
            Do::Run(Target::Action(Action::NewTerminal)),
        )],
    );
    add(
        "notes",
        "desklink-edit-note-symbolic",
        "Notes on both",
        "Write a note here and it is on the phone too, and the other way round.".into(),
        Needs::Nothing,
        vec![("New note".into(), Do::Run(Target::Action(Action::NewNote)))],
    );
    add(
        "ring",
        "desklink-ring-volume-symbolic",
        "Find my phone",
        format!("Make {name} ring so you can find it, even when it is on silent."),
        Needs::Ready(Action::Ring),
        on_phone(Action::Ring),
    );
    add(
        "lock",
        "desklink-lock-symbolic",
        "Lock and unlock",
        "Lock this computer from the phone. To unlock it with the phone too, turn that on \
         in Privacy & security."
            .into(),
        Needs::Pair,
        vec![(
            "Privacy & security".into(),
            Do::Run(settings("Privacy & security")),
        )],
    );
    add(
        "glide",
        "desklink-mouse-symbolic",
        "Glide: the mouse moves onto the phone",
        format!("Push the mouse past the edge of this screen and it keeps going onto {name}."),
        Needs::Pair,
        vec![("Glide settings".into(), Do::Run(settings("Control")))],
    );
    add(
        "widgets",
        "desklink-device-phone-symbolic",
        "Widgets and Quick Settings tiles",
        "On your phone: touch and hold the home screen to add DeskLink widgets, and edit \
         Quick Settings to add its tiles."
            .into(),
        Needs::Nothing,
        Vec::new(),
    );
    let top_bar = "DeskLink in GNOME's top bar: send files, ring or lock in one click.";
    let (how, not_now, buttons) = match facts.extension {
        None => (
            top_bar.to_string(),
            Some("Only on GNOME. Here, use this window or DeskLink's tray icon.".to_string()),
            Vec::new(),
        ),
        Some(ExtensionStatus::Off) => (
            format!("{top_bar} {}", requirements::EXTENSION_NOTE),
            None,
            vec![("Turn on".to_string(), Do::Fix(Fix::TurnOnExtension))],
        ),
        Some(ExtensionStatus::NeedsLogin) => (
            top_bar.to_string(),
            Some("Log out and back in to see it.".to_string()),
            vec![("Log out…".to_string(), Do::Fix(Fix::LogOut))],
        ),
        Some(ExtensionStatus::Active) => (format!("{top_bar} It is on."), None, Vec::new()),
    };
    out.push(Card {
        id: "top-bar",
        icon: "desklink-more-horiz-symbolic",
        title: "Top-bar menu",
        how,
        not_now,
        buttons,
    });
    let mut how = "Put both on the same Wi-Fi and keep DeskLink open on the phone. On the \
                   phone, turn off battery limits for DeskLink: its Get started guide has \
                   the button."
        .to_string();
    let mut buttons = Vec::new();
    if let Some(phone) = phone.filter(|phone| phone.reach == Reach::Offline) {
        buttons.push((
            "Try again".to_string(),
            Do::Fix(Fix::Reconnect(phone.id.clone())),
        ));
    }
    buttons.push(("Search for phones".to_string(), Do::Discover));
    if let Some(command) = &facts.firewall {
        how.push_str(
            " If this computer has a firewall, let DeskLink through: copy the command and \
             run it in a terminal.",
        );
        buttons.push((
            "Copy firewall command".to_string(),
            Do::Fix(Fix::Copy(command.clone())),
        ));
    }
    out.push(Card {
        id: "trouble",
        icon: "desklink-wifi-symbolic",
        title: "Can't connect?",
        how,
        not_now: None,
        buttons,
    });
    out
}

/// The command that opens DeskLink's ports (TCP 1716 to 1764, UDP 1716 and
/// mDNS) in the firewall this computer has, if it has one we know.
fn firewall_command() -> Option<String> {
    // ponytail: the ports are `daemon::network`'s; keep them in step if they move.
    if glib::find_program_in_path("firewall-cmd").is_some() {
        Some(
            "sudo firewall-cmd --permanent --add-port=1716-1764/tcp --add-port=1716/udp \
             --add-service=mdns && sudo firewall-cmd --reload"
                .into(),
        )
    } else if glib::find_program_in_path("ufw").is_some() {
        Some(
            "sudo ufw allow 1716:1764/tcp && sudo ufw allow 1716/udp && sudo ufw allow \
             5353/udp"
                .into(),
        )
    } else {
        None
    }
}

impl Facts {
    /// This computer and the selected paired phone, as they are now.
    pub(super) fn now(window: &DesklinkWindow) -> Self {
        let paired = window.paired_devices();
        let selected = window.selected_id();
        let phone = paired
            .iter()
            .find(|device| Some(&device.id) == selected.as_ref())
            .or_else(|| paired.first())
            .map(PhoneFacts::from_device);
        Self {
            phone,
            extension: if cfg!(test) {
                None
            } else {
                crate::shell_dbus::extension_possible().then(crate::shell_dbus::extension_status)
            },
            displays: glib::find_program_in_path("gnome-control-center").is_some(),
            firewall: firewall_command(),
        }
    }
}

/// Does what a card's button says.
pub(super) fn run(window: &DesklinkWindow, what: &Do) {
    match what {
        Do::Run(target) => super::palette::run(window, target),
        Do::Fix(fix) => requirements::run(
            window,
            &requirements::Need {
                id: "guide",
                on_phone: false,
                title: String::new(),
                detail: String::new(),
                fix: fix.clone(),
                allow: None,
            },
        ),
        Do::Discover => {
            gtk::prelude::WidgetExt::activate_action(window, "win.discover", None).ok();
        }
        Do::Displays => {
            if let Some(settings) = glib::find_program_in_path("gnome-control-center") {
                let _ = std::process::Command::new(settings).arg("display").spawn();
            }
        }
    }
}

fn label(text: &str, classes: &[&str]) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .xalign(0.0)
        .css_classes(classes)
        .build()
}

/// One card's widget; a button closes the guide, then does its thing.
fn card_widget(card: &Card, dialog: &adw::Dialog, window: &DesklinkWindow) -> gtk::Box {
    let widget = ui_kit::card(None);
    widget.set_widget_name(card.id);
    widget.add_css_class("dl-guide-card");
    let head = gtk::Box::builder().spacing(14).build();
    let square = ui_kit::icon_square(card.icon, Tone::Accent);
    square.set_valign(gtk::Align::Start);
    head.append(&square);
    let words = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(4)
        .hexpand(true)
        .build();
    let title = label(card.title, &["dl-card-title"]);
    title.set_accessible_role(gtk::AccessibleRole::Heading);
    title.update_property(&[gtk::accessible::Property::Level(2)]);
    words.append(&title);
    words.append(&label(&card.how, &["dl-secondary"]));
    if let Some(why) = &card.not_now {
        let line = gtk::Box::builder()
            .spacing(6)
            .css_classes(["dl-guide-not-now"])
            .build();
        let glyph = gtk::Image::builder()
            .icon_name("desklink-info-symbolic")
            .pixel_size(16)
            .valign(gtk::Align::Start)
            .accessible_role(gtk::AccessibleRole::Presentation)
            .build();
        line.append(&glyph);
        line.append(&label(why, &[]));
        words.append(&line);
    }
    head.append(&words);
    widget.append(&head);
    if !card.buttons.is_empty() {
        let row = gtk::FlowBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .homogeneous(false)
            .max_children_per_line(3)
            .column_spacing(8)
            .row_spacing(8)
            .halign(gtk::Align::Start)
            // Under the words, not the icon (40 px square, 14 px gap).
            .margin_start(54)
            .build();
        for (index, (words, what)) in card.buttons.iter().enumerate() {
            let kind = if index == 0 {
                ButtonKind::Tonal
            } else {
                ButtonKind::Outline
            };
            let button = ui_kit::button(kind, words, None);
            button.set_widget_name(&format!("guide-{}-{index}", card.id));
            button.update_property(&[gtk::accessible::Property::Description(card.title)]);
            let (what, dialog, window) = (what.clone(), dialog.downgrade(), window.downgrade());
            button.connect_clicked(move |_| {
                if let Some(dialog) = dialog.upgrade() {
                    dialog.close();
                }
                if let Some(window) = window.upgrade() {
                    run(&window, &what);
                }
            });
            super::transfers::add_card(&row, &button);
        }
        widget.append(&row);
    }
    widget
}

/// Opens the guide over `window`.
pub(super) fn show(window: &DesklinkWindow) {
    let cards = cards(&Facts::now(window));
    let list = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .css_classes(["dl-guide"])
        .build();
    let intro = label(SUBTITLE, &["dl-secondary"]);
    list.append(&intro);
    let dialog = adw::Dialog::builder()
        .title(TITLE)
        .content_width(720)
        .content_height(760)
        .build();
    for card in &cards {
        list.append(&card_widget(card, &dialog, window));
    }
    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&adw::Clamp::builder().maximum_size(680).child(&list).build())
        .build();
    let view = adw::ToolbarView::builder().content(&scroll).build();
    view.add_top_bar(&adw::HeaderBar::new());
    dialog.set_child(Some(&view));
    dialog.set_widget_name("guide");
    dialog.present(Some(window));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn phone(reach: Reach) -> PhoneFacts {
        PhoneFacts {
            id: "p".into(),
            name: "Pixel 9".into(),
            reach,
            incoming: crate::device_links::features::FeatureRegistry::desktop()
                .outgoing_capabilities()
                .to_vec(),
            ..PhoneFacts::default()
        }
    }

    fn facts(phone: Option<PhoneFacts>) -> Facts {
        Facts {
            phone,
            extension: Some(ExtensionStatus::Off),
            displays: true,
            firewall: Some("sudo ufw allow".into()),
        }
    }

    /// The cards and their titles are the phone's, in the phone's order
    /// (`COPY.txt`, the desk's wording of the first).
    #[test]
    fn the_cards_are_the_shared_list() {
        let titles: Vec<&str> = cards(&facts(None)).iter().map(|card| card.title).collect();
        assert_eq!(
            titles,
            [
                "Pair a phone",
                "Send a file",
                "Copy on one, paste on the other",
                "See your computer's screen on the phone",
                "Use the phone as a second screen",
                "Share your phone's screen to the computer",
                "Phone notifications on the computer",
                "Terminals on both",
                "Notes on both",
                "Find my phone",
                "Lock and unlock",
                "Glide: the mouse moves onto the phone",
                "Widgets and Quick Settings tiles",
                "Top-bar menu",
                "Can't connect?",
            ]
        );
        assert_eq!(TITLE, "Get started");
        assert_eq!(SUBTITLE, "What DeskLink can do, and how to start each one.");
    }

    /// Never hidden: without a phone what needs one says so and offers
    /// pairing; an unreachable phone says so and offers Try again; a ready
    /// phone gets the button that does it.
    #[test]
    fn what_cannot_run_says_why_with_the_fix() {
        let unpaired = cards(&facts(None));
        assert_eq!(unpaired.len(), 15);
        for id in [
            "send",
            "clipboard",
            "desk-screen",
            "phone-screen",
            "ring",
            "lock",
            "glide",
        ] {
            let card = unpaired.iter().find(|card| card.id == id).unwrap();
            assert_eq!(card.not_now.as_deref(), Some("Pair a phone first."), "{id}");
            assert_eq!(
                card.buttons,
                [pair_button(false)],
                "{id}: the fix is to pair"
            );
        }
        let away = cards(&facts(Some(phone(Reach::Offline))));
        let send = away.iter().find(|card| card.id == "send").unwrap();
        assert!(send
            .not_now
            .as_deref()
            .unwrap()
            .starts_with("Can't reach Pixel 9. "));
        assert_eq!(send.buttons[0].0, "Try again");
        let trouble = away.iter().find(|card| card.id == "trouble").unwrap();
        assert_eq!(trouble.buttons[0].0, "Try again");
        let ready = cards(&facts(Some(phone(Reach::Ready))));
        let send = ready.iter().find(|card| card.id == "send").unwrap();
        assert!(send.not_now.is_none());
        assert_eq!(
            send.buttons,
            [(
                "Choose files".to_string(),
                Do::Run(Target::PhoneAction("p".into(), Action::SendFiles))
            )]
        );
        let pair = &ready[0];
        assert_eq!(pair.buttons, [pair_button(true)]);
        assert!(pair.how.starts_with("Paired with Pixel 9."));
    }

    /// Every button goes somewhere real: a Settings page that exists, a page
    /// of the window, an action the palette runs, a fix with a button.
    /// Only the phone-side widgets card and a top bar that is already on
    /// have no button, and those say where instead.
    #[test]
    fn every_button_resolves_to_a_real_place() {
        let states = [
            facts(None),
            facts(Some(phone(Reach::Ready))),
            facts(Some(phone(Reach::Offline))),
            facts(Some(phone(Reach::Connecting))),
            Facts {
                extension: Some(ExtensionStatus::NeedsLogin),
                ..facts(None)
            },
        ];
        for facts in &states {
            for card in cards(facts) {
                for (words, what) in &card.buttons {
                    assert!(!words.is_empty());
                    match what {
                        Do::Run(Target::Settings(index)) => {
                            assert!(*index < super::super::prefs::GROUPS.len());
                            assert_ne!(*index, 0, "{words}: a named page, not the first");
                        }
                        Do::Run(Target::Page(page)) => {
                            assert!(super::super::PAGES.iter().any(|(name, _, _)| name == page))
                        }
                        Do::Run(Target::Action(_) | Target::PhoneAction(..)) => {}
                        Do::Fix(fix) => assert!(fix.label().is_some(), "{words}"),
                        Do::Discover | Do::Displays => {}
                        Do::Run(other) => panic!("{other:?}"),
                    }
                }
                let connecting = facts.phone.as_ref().map(|p| p.reach) == Some(Reach::Connecting);
                if card.buttons.is_empty() && !connecting {
                    assert!(
                        matches!(card.id, "widgets" | "top-bar"),
                        "{}: a button or where to go",
                        card.id
                    );
                }
            }
        }
        let off_gnome = cards(&Facts {
            extension: None,
            ..facts(None)
        });
        let top = off_gnome.iter().find(|card| card.id == "top-bar").unwrap();
        assert!(top.not_now.as_deref().unwrap().starts_with("Only on GNOME"));
    }

    /// Plain words, and nothing promises use off this network (1.3).
    #[test]
    fn the_words_are_plain() {
        for facts in [facts(None), facts(Some(phone(Reach::Offline)))] {
            for card in cards(&facts) {
                let mut all = vec![card.title.to_string(), card.how.clone()];
                all.extend(card.not_now.clone());
                all.extend(card.buttons.iter().map(|(words, _)| words.clone()));
                for words in all {
                    super::super::onboarding::assert_plain(&words);
                }
            }
        }
    }

    // ---- Widget tests (on the dedicated test display) -----------------------

    use adw::subclass::prelude::ObjectSubclassIsExt;

    fn window() -> (
        DesklinkWindow,
        std::sync::mpsc::Receiver<crate::device_links::daemon::DaemonCommand>,
    ) {
        let resources = crate::load_resources().expect("the bundled resources");
        gio::resources_register(&resources);
        let app = adw::Application::builder()
            .application_id("derx06.desklink.com.test")
            .build();
        let window = DesklinkWindow::new(&app);
        let (daemon, commands) = crate::device_links::daemon::DaemonHandle::detached();
        daemon.set_load_state_for_tests(crate::device_links::daemon::handle::LOAD_KNOWN);
        window.set_daemon(daemon);
        (window, commands)
    }

    fn guide_open(window: &DesklinkWindow) -> bool {
        window
            .visible_dialog()
            .is_some_and(|dialog| dialog.widget_name() == "guide")
    }

    fn pump() {
        let context = glib::MainContext::default();
        while context.iteration(false) {}
    }

    /// The main menu, Find anything and Settings each open it, and a card's
    /// button closes it and does what it says.
    #[test]
    fn the_guide_opens_from_the_menu_the_palette_and_settings() {
        assert!(
            include_str!("../window.ui")
                .contains("<attribute name=\"action\">win.guide</attribute>"),
            "in the main menu"
        );
        assert!(super::super::palette_model::offered_actions(None).contains(&Action::Guide));
        crate::test_display::run(|| {
            let (window, _commands) = window();
            window.present();
            window.refresh_devices();
            // Past the first-run tour (it has no menu): the pairing page.
            window.end_onboarding(false, false);
            gtk::prelude::WidgetExt::activate_action(&window, "win.guide", None)
                .expect("the menu's action");
            assert!(guide_open(&window), "from the menu");
            let cards = super::super::named_widgets(
                window.visible_dialog().unwrap().upcast_ref(),
                "guide-",
            );
            assert!(!cards.is_empty(), "buttons on the cards");
            window.visible_dialog().unwrap().close();
            pump();

            super::super::palette::run(&window, &Target::Action(Action::Guide));
            assert!(guide_open(&window), "from Find anything");
            // "New note" leaves the guide for a note to type in.
            let new_note = super::super::named_widgets(
                window.visible_dialog().unwrap().upcast_ref(),
                "guide-notes-0",
            )
            .into_iter()
            .next()
            .and_downcast::<gtk::Button>()
            .expect("the Notes card's button");
            new_note.emit_clicked();
            pump();
            assert!(!guide_open(&window), "the guide closed");
            assert_eq!(
                window.imp().main_stack.visible_child_name().as_deref(),
                Some("solo"),
                "a new note"
            );

            window.leave_solo();
            let phone = super::super::debug_fake_device("paired", "Pixel 9");
            window.imp().fake_devices.borrow_mut().push(phone);
            window.refresh_devices();
            assert!(window.show_page("settings"));
            pump();
            let row = super::super::named_widgets(window.upcast_ref(), "settings-guide")
                .into_iter()
                .next()
                .and_downcast::<adw::ActionRow>()
                .expect("Settings' Get started row");
            adw::prelude::ActionRowExt::activate(&row);
            pump();
            assert!(guide_open(&window), "from Settings");
            window.close();
        });
    }
}
